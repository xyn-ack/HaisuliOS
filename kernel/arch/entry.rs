// SPDX-License-Identifier: GPL-2.0

//! Kernel entry points.
//!
//! Every way into and out of the kernel: exception, interrupt and system call entry stubs.
//!
//! The stubs save the interrupted state and call ordinary (safe) Rust handlers, which they
//! reference with `sym` operands: [`crate::idt::exception_handler`], [`crate::irq::irq_dispatch`]
//! and [`crate::syscall::syscall_dispatch`]. `trap_return` is the common way back to user mode.

use super::cpu;
use super::x86::{Efer, Msr, RFlag};
use crate::gdt::{Selector, USER_CS, USER_DS};
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Saved user context; lowest address first (matches the push order of
/// `syscall_entry` and the pop order of `trap_return`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    // The `iretq` frame.
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl TrapFrame {
    pub(crate) const WORDS: usize = 20;

    /// The frame as it lies in memory, lowest address first.
    pub(crate) fn words(&self) -> [u64; Self::WORDS] {
        [
            self.r15,
            self.r14,
            self.r13,
            self.r12,
            self.r11,
            self.r10,
            self.r9,
            self.r8,
            self.rbp,
            self.rdi,
            self.rsi,
            self.rdx,
            self.rcx,
            self.rbx,
            self.rax,
            self.rip,
            self.cs,
            self.rflags,
            self.rsp,
            self.ss,
        ]
    }

    /// The frame that [`words`](Self::words) returned.
    pub(crate) fn from_words(w: &[u64; Self::WORDS]) -> TrapFrame {
        let [r15, r14, r13, r12, r11, r10, r9, r8, rbp, rdi, rsi, rdx, rcx, rbx, rax, rip, cs, rflags, rsp, ss] =
            *w;
        TrapFrame {
            r15,
            r14,
            r13,
            r12,
            r11,
            r10,
            r9,
            r8,
            rbp,
            rdi,
            rsi,
            rdx,
            rcx,
            rbx,
            rax,
            rip,
            cs,
            rflags,
            rsp,
            ss,
        }
    }
}

/// Stack contents when `idt::exception_handler` is called (lowest address first).
#[repr(C)]
pub(crate) struct ExceptionFrame {
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

macro_rules! stub_table {
    ($(#[$doc:meta])* $table:ident: [$($stub:ident),* $(,)?]) => {
        extern "C" {
            $(fn $stub();)*
        }
        $(#[$doc])*
        pub(crate) static $table: [unsafe extern "C" fn(); [$(stringify!($stub)),*].len()] =
            [$($stub),*];
    };
}

stub_table!(
    /// Entry stubs for CPU exceptions 0..31, by vector.
    EXCEPTION_STUBS: [
        isr0, isr1, isr2, isr3, isr4, isr5, isr6, isr7, isr8, isr9, isr10, isr11, isr12, isr13,
        isr14, isr15, isr16, isr17, isr18, isr19, isr20, isr21, isr22, isr23, isr24, isr25, isr26,
        isr27, isr28, isr29, isr30, isr31,
    ]
);

stub_table!(
    /// Entry stubs for hardware interrupt lines 0..15.
    IRQ_STUBS: [
        irq_stub0, irq_stub1, irq_stub2, irq_stub3, irq_stub4, irq_stub5, irq_stub6,
        irq_stub7, irq_stub8, irq_stub9, irq_stub10, irq_stub11, irq_stub12, irq_stub13,
        irq_stub14, irq_stub15,
    ]
);

extern "C" {
    fn syscall_entry();
}

/// Address of an entry stub, for an IDT gate.
pub(crate) fn stub_address(stub: unsafe extern "C" fn()) -> u64 {
    stub as usize as u64
}

// ---- CPU exceptions ------------------------------------------------------------

global_asm!(
    r#"
.macro ISR_NOERR n
.global isr\n
isr\n:
    push 0
    push \n
    jmp isr_common
.endm

.macro ISR_ERR n
.global isr\n
isr\n:
    push \n
    jmp isr_common
.endm

.pushsection .text
# The CPU pushes an error code for these exceptions only (SDM table 6-1; see
# `x86::Exception`): #DF 8, #TS 10, #NP 11, #SS 12, #GP 13, #PF 14, #AC 17,
# #CP 21, #VC 29 and #SX 30. For the others the stub pushes a 0 in its place,
# so that every handler finds the same frame.
.irp n, 0,1,2,3,4,5,6,7,9,15,16,18,19,20,22,23,24,25,26,27,28,31
    ISR_NOERR \n
.endr
.irp n, 8,10,11,12,13,14,17,21,29,30
    ISR_ERR \n
.endr

isr_common:
    cld
    mov rdi, rsp                # &ExceptionFrame
    sub rsp, 8                  # keep the SysV 16-byte alignment at the call
    call {handler}
    ud2
.popsection
"#,
    handler = sym crate::idt::exception_handler,
);

// ---- Hardware interrupts -----------------------------------------------------------

// The CPU has pushed ss, rsp, rflags, cs, rip (no error code). Save every
// general-purpose register: the interrupted code may be user mode or the
// kernel's idle loop, and neither expects anything to change.
global_asm!(
    r#"
.macro IRQ_STUB n
.global irq_stub\n
irq_stub\n:
    push \n
    jmp irq_common
.endm

.pushsection .text
.irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    IRQ_STUB \n
.endr

irq_common:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    cld
    mov rdi, [rsp + 15*8]       # line number pushed by the stub
    sub rsp, 8                  # 16-byte alignment for the call
    call {dispatch}
    add rsp, 8
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 8                  # drop the line number
    iretq
.popsection
"#,
    dispatch = sym crate::irq::irq_dispatch,
);

// ---- System calls ----------------------------------------------------------------

/// Single CPU, so one scratch slot for the user rsp is enough.
static SYSCALL_USER_RSP: AtomicU64 = AtomicU64::new(0);
/// Kernel stack top of the running task.
static SYSCALL_KERNEL_RSP: AtomicU64 = AtomicU64::new(0);

/// Kernel stack to switch to on `syscall` (the running task's).
pub(crate) fn set_syscall_stack(top: u64) {
    SYSCALL_KERNEL_RSP.store(top, Ordering::Relaxed);
}

/// STAR: `syscall` loads the kernel code segment from bits 32..48 (and the
/// kernel data segment from the next GDT entry). `sysret` loads the user data
/// segment from the entry after the selector in bits 48..64, and user code
/// from the one after that. (HaisuliOS returns with `iretq`, but `sysret`
/// would work.)
const STAR: u64 = (Selector::UserData as u64 - 8) << 48 | (Selector::KernelCode as u64) << 32;
const _: () = {
    // The GDT layout that `syscall` and `sysret` assume.
    assert!(Selector::KernelData as u64 == Selector::KernelCode as u64 + 8);
    assert!(Selector::UserCode as u64 == Selector::UserData as u64 + 8);
};

/// RFLAGS bits that `syscall` clears: single-stepping, interrupts (the kernel
/// runs with them off) and the direction flag (the ABI wants it clear).
const SYSCALL_CLEARS: u64 = RFlag::bits(&[RFlag::Trap, RFlag::Interrupt, RFlag::Direction]);

/// Enables the `syscall` instruction and points it at `syscall_entry`.
pub(crate) fn init_syscalls() {
    let efer = cpu::rdmsr(Msr::Efer) | Efer::Syscall as u64;
    cpu::wrmsr(Msr::Efer, efer);
    cpu::wrmsr(Msr::Star, STAR);
    cpu::wrmsr(Msr::LStar, stub_address(syscall_entry));
    cpu::wrmsr(Msr::SfMask, SYSCALL_CLEARS);
}

// We always return to user mode through iretq (trap_return), never sysret.
global_asm!(
    r#"
.pushsection .text
.global syscall_entry
syscall_entry:
    mov [rip + {user_rsp}], rsp
    mov rsp, [rip + {kernel_rsp}]
    push {user_ss}                             # ss
    push qword ptr [rip + {user_rsp}]          # rsp
    push r11                                   # rflags
    push {user_cs}                             # cs
    push rcx                                   # rip
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    mov rdi, rsp                               # &mut TrapFrame
    call {dispatch}
    jmp trap_return

# Pop a TrapFrame and return to user mode. Also the first "return address"
# of every new task (see context::init).
.global trap_return
trap_return:
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    iretq
.popsection
"#,
    user_rsp = sym SYSCALL_USER_RSP,
    kernel_rsp = sym SYSCALL_KERNEL_RSP,
    user_ss = const USER_DS,
    user_cs = const USER_CS,
    dispatch = sym crate::syscall::syscall_dispatch,
);
