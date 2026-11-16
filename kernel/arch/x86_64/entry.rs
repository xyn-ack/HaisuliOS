// SPDX-License-Identifier: GPL-2.0

//! Kernel entry points.
//!
//! Every way into and out of the kernel: exception, interrupt and system call entry stubs.
//!
//! The stubs save the interrupted state and call ordinary (safe) Rust handlers, which they
//! reference with `sym` operands: [`crate::idt::exception_handler`],
//! [`crate::irq::interrupt_dispatch`] and [`crate::syscall::syscall_dispatch`]. `trap_return` is
//! the common way back to user mode after a system call.
//!
//! Every entry from user mode executes `swapgs` first, and every return to user mode last, so
//! that kernel code always finds its CPU's [`super::percpu`] block through GS (see there).

use super::cpu;
use super::gdt::{Selector, USER_CS, USER_DS};
use super::percpu;
use super::x86::{Efer, Msr, RFlag};
use crate::arch::UserRegs;
use core::arch::global_asm;

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

/// Bytes of the `syscall` instruction, to make an interrupted call again.
const SYSCALL_LEN: u64 = 2;

/// RFLAGS of a thread entering user mode: interrupts enabled (and the reserved bit 1).
pub(crate) const USER_RFLAGS: u64 = RFlag::bits(&[RFlag::Interrupt, RFlag::Reserved1]);

// What generic code may do with the saved registers (see [`crate::arch::UserRegs`]): the
// x86_64 system call convention, and `syscall`'s return path through rcx and r11.
impl UserRegs for TrapFrame {
    fn new_program(entry: u64, sp: u64) -> TrapFrame {
        TrapFrame {
            rip: entry,
            cs: USER_CS,
            rflags: USER_RFLAGS,
            rsp: sp,
            ss: USER_DS,
            ..TrapFrame::default()
        }
    }

    fn in_user_mode(&self) -> bool {
        use super::x86::PrivilegeLevel;
        PrivilegeLevel::of_selector(self.cs) == PrivilegeLevel::User
    }

    fn syscall_nr(&self) -> u64 {
        self.rax
    }

    fn syscall_args(&self) -> [u64; 6] {
        [self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9]
    }

    fn set_return(&mut self, value: i64) {
        self.rax = value as u64;
    }

    fn returned(&self) -> i64 {
        self.rax as i64
    }

    fn restart_syscall(&mut self, nr: u64) {
        // Back up to the `syscall` instruction, with the number in place again.
        self.rip = self.rip.wrapping_sub(SYSCALL_LEN);
        self.rax = nr;
    }

    fn sp(&self) -> u64 {
        self.rsp
    }

    fn set_sp(&mut self, sp: u64) {
        self.rsp = sp;
    }
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
    /// Entry stubs for hardware interrupt vectors 32..48 (the PICs' lines 0..15).
    IRQ_STUBS: [
        irq_stub32, irq_stub33, irq_stub34, irq_stub35, irq_stub36, irq_stub37, irq_stub38,
        irq_stub39, irq_stub40, irq_stub41, irq_stub42, irq_stub43, irq_stub44, irq_stub45,
        irq_stub46, irq_stub47,
    ]
);

stub_table!(
    /// Entry stubs for the local APIC's vectors: the timer, the reschedule IPI and spurious
    /// interrupts (see [`super::apic`]), in that order.
    APIC_STUBS: [irq_stub240, irq_stub241, irq_stub255]
);

// The stubs' names (and the `.irp` list below) spell out the APIC's vectors.
const _: () = assert!(
    super::apic::TIMER_VECTOR == 240
        && super::apic::RESCHEDULE_VECTOR == 241
        && super::apic::SPURIOUS_VECTOR == 255
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
    test byte ptr [rsp + 24], 3 # from user mode (the CPL of the saved cs)?
    jz 1f
    swapgs
1:
    cld
    mov rdi, rsp                # &ExceptionFrame
    sub rsp, 8                  # keep the SysV 16-byte alignment at the call
    call {handler}
    ud2
.popsection
"#,
    handler = sym super::idt::exception_handler,
);

// ---- Hardware interrupts -----------------------------------------------------------

// The CPU has pushed ss, rsp, rflags, cs, rip (no error code). The stub saves
// rax and passes the vector in it; the common part saves the other registers,
// which makes a `TrapFrame`. Every register is restored: the interrupted code
// may be user mode or the kernel's idle loop, and neither expects anything to
// change. From user mode, the handler may switch to another thread before it
// returns (preemption), so it gets the frame.
global_asm!(
    r#"
.macro IRQ_STUB n
.global irq_stub\n
irq_stub\n:
    push rax
    mov eax, \n
    jmp irq_common
.endm

.pushsection .text
.irp n, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,240,241,255
    IRQ_STUB \n
.endr

irq_common:
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
    test byte ptr [rsp + 16*8], 3  # from user mode (the CPL of the saved cs)?
    jz 1f
    swapgs
1:
    cld
    mov edi, eax                # the vector
    mov rsi, rsp                # &mut TrapFrame (20 words: rsp stays 16-byte aligned)
    call {dispatch}
    test byte ptr [rsp + 16*8], 3
    jz 2f
    swapgs
2:
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
    dispatch = sym crate::irq::interrupt_dispatch,
);

// ---- System calls ----------------------------------------------------------------

/// Kernel stack to switch to on `syscall` on this CPU (the running thread's).
pub(crate) fn set_syscall_stack(top: u64) {
    percpu::set_syscall_stack(top);
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

/// Enables the `syscall` instruction on this CPU and points it at `syscall_entry`.
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
    swapgs
    mov gs:[{user_rsp}], rsp
    mov rsp, gs:[{kernel_rsp}]
    push {user_ss}                             # ss
    push qword ptr gs:[{user_rsp}]             # rsp
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
# of every new thread (see context::init).
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
    swapgs
    iretq
.popsection
"#,
    user_rsp = const percpu::USER_RSP,
    kernel_rsp = const percpu::KERNEL_RSP,
    user_ss = const USER_DS,
    user_cs = const USER_CS,
    dispatch = sym crate::syscall::syscall_dispatch,
);
