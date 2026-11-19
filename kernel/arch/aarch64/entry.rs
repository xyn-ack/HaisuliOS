// SPDX-License-Identifier: MIT

//! Kernel entry points.
//!
//! Every way into and out of the kernel: the exception vectors (`VBAR_EL1`), which handle system
//! calls (`svc`), interrupts and faults, and `trap_return`, the way back.
//!
//! Every exception saves the complete register state in a [`TrapFrame`] on the stack and calls an
//! ordinary (safe) Rust handler: [`el0_sync`] (a system call, or a fault of user code), [`irq`]
//! (which tells the generic [`crate::irq::dispatch`] where the interrupt came from), or a panic
//! for what must not happen. The frame includes the FP/SIMD registers: the kernel's own code uses
//! them too, so user code would otherwise find them changed.
//!
//! The kernel runs on `SP_EL1`. While a thread is in user mode, `SP_EL1` is the top of its
//! kernel stack (where the return to user mode left it), so the frame of an exception from EL0
//! is always at the top of the thread's kernel stack, as [`super::context`] expects.

use super::{cpu, gic, percpu, timer};
use crate::arch::{Interrupt, UserRegs};
use crate::task::{self, Why};
use core::arch::global_asm;
use core::fmt::Write;

/// Saved user context (or kernel context, for an interrupt of an idle CPU); lowest address
/// first, as the vector code saves it.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct TrapFrame {
    /// `x0` to `x30`.
    pub x: [u64; 31],
    /// The user stack pointer (`SP_EL0`).
    pub sp: u64,
    /// Where the exception came from (`ELR_EL1`): the return address.
    pub elr: u64,
    /// The interrupted processor state (`SPSR_EL1`).
    pub spsr: u64,
    /// `x0` as it was on entry: the first argument of a system call, which the result
    /// overwrites (a restart puts it back).
    pub orig_x0: u64,
    _pad: u64,
    /// `q0` to `q31`, two words each (low half first).
    pub v: [u64; 64],
    /// The FP control and status registers.
    pub fpcr: u64,
    pub fpsr: u64,
}

/// Bytes of a [`TrapFrame`], and where its FP/SIMD part starts (for the vector code).
const FRAME_SIZE: usize = size_of::<TrapFrame>();
const FP_OFFSET: usize = core::mem::offset_of!(TrapFrame, v);
const _: () = assert!(FRAME_SIZE == 816 && FP_OFFSET == 288 && FRAME_SIZE.is_multiple_of(16));
const _: () = assert!(core::mem::offset_of!(TrapFrame, orig_x0) == 272);

/// SPSR_EL1 of a thread in user mode: EL0 with its own stack pointer (EL0t), every interrupt
/// enabled (DAIF clear).
pub(crate) const USER_SPSR: u64 = 0;
/// SPSR_EL1's mode field: the exception level and stack pointer the CPU was interrupted in.
const SPSR_MODE: u64 = 0b1_1111;
/// SPSR_EL1's condition flags (N, Z, C, V): what user code may change of it.
pub(crate) const SPSR_FLAGS: u64 = 0b1111 << 28;

/// Bytes of the `svc` instruction, to make an interrupted call again.
const SVC_LEN: u64 = 4;

/// Exception classes (ESR_EL1 bits 26..32) that the kernel tells apart.
mod ec {
    pub(super) const UNKNOWN: u64 = 0x00;
    pub(super) const SVC64: u64 = 0x15;
    pub(super) const INSTRUCTION_ABORT_EL0: u64 = 0x20;
    pub(super) const INSTRUCTION_ABORT_EL1: u64 = 0x21;
    pub(super) const PC_ALIGNMENT: u64 = 0x22;
    pub(super) const DATA_ABORT_EL0: u64 = 0x24;
    pub(super) const DATA_ABORT_EL1: u64 = 0x25;
    pub(super) const SP_ALIGNMENT: u64 = 0x26;
    pub(super) const BRK: u64 = 0x3C;
}

/// A human-readable name for exception class `class`.
fn class_name(class: u64) -> &'static str {
    match class {
        ec::UNKNOWN => "undefined instruction",
        // Translation, access flag and permission faults alike: what x86 calls a page fault.
        ec::INSTRUCTION_ABORT_EL0
        | ec::INSTRUCTION_ABORT_EL1
        | ec::DATA_ABORT_EL0
        | ec::DATA_ABORT_EL1 => "page fault",
        ec::PC_ALIGNMENT => "PC alignment fault",
        ec::SP_ALIGNMENT => "SP alignment fault",
        ec::BRK => "breakpoint",
        _ => "exception",
    }
}

/// Whether exception class `class` has a faulting address in FAR_EL1.
fn has_fault_address(class: u64) -> bool {
    matches!(
        class,
        ec::INSTRUCTION_ABORT_EL0
            | ec::INSTRUCTION_ABORT_EL1
            | ec::DATA_ABORT_EL0
            | ec::DATA_ABORT_EL1
            | ec::PC_ALIGNMENT
    )
}

// What generic code may do with the saved registers (see [`crate::arch::UserRegs`]): the arm64
// system call convention, `x8` for the number and `x0` to `x5` for the arguments and the result.
impl UserRegs for TrapFrame {
    fn new_program(entry: u64, sp: u64) -> TrapFrame {
        TrapFrame {
            elr: entry,
            sp,
            spsr: USER_SPSR,
            ..TrapFrame::ZERO
        }
    }

    fn in_user_mode(&self) -> bool {
        self.spsr & SPSR_MODE == 0 // EL0t
    }

    fn syscall_nr(&self) -> u64 {
        self.x[8]
    }

    fn syscall_args(&self) -> [u64; 6] {
        [
            self.x[0], self.x[1], self.x[2], self.x[3], self.x[4], self.x[5],
        ]
    }

    fn set_return(&mut self, value: i64) {
        self.x[0] = value as u64;
    }

    fn returned(&self) -> i64 {
        self.x[0] as i64
    }

    fn restart_syscall(&mut self, nr: u64) {
        // Back up to the `svc` instruction, with the number and first argument in place again.
        self.elr = self.elr.wrapping_sub(SVC_LEN);
        self.x[8] = nr;
        self.x[0] = self.orig_x0;
    }

    fn sp(&self) -> u64 {
        self.sp
    }

    fn set_sp(&mut self, sp: u64) {
        self.sp = sp;
    }
}

impl TrapFrame {
    /// Every register 0.
    pub(crate) const ZERO: TrapFrame = TrapFrame {
        x: [0; 31],
        sp: 0,
        elr: 0,
        spsr: 0,
        orig_x0: 0,
        _pad: 0,
        v: [0; 64],
        fpcr: 0,
        fpsr: 0,
    };

    pub(crate) const WORDS: usize = FRAME_SIZE / 8;

    /// The frame as it lies in memory, lowest address first.
    pub(crate) fn words(&self) -> [u64; Self::WORDS] {
        let mut w = [0; Self::WORDS];
        w[..31].copy_from_slice(&self.x);
        w[31..36].copy_from_slice(&[self.sp, self.elr, self.spsr, self.orig_x0, self._pad]);
        w[36..100].copy_from_slice(&self.v);
        w[100] = self.fpcr;
        w[101] = self.fpsr;
        w
    }

    /// The frame that [`words`](Self::words) returned.
    pub(crate) fn from_words(w: &[u64; Self::WORDS]) -> TrapFrame {
        let mut f = TrapFrame::ZERO;
        f.x.copy_from_slice(&w[..31]);
        [f.sp, f.elr, f.spsr, f.orig_x0, f._pad] = [w[31], w[32], w[33], w[34], w[35]];
        f.v.copy_from_slice(&w[36..100]);
        f.fpcr = w[100];
        f.fpsr = w[101];
        f
    }
}

// The vectors: 16 entries of 128 bytes, by where the exception came from (the current EL with
// SP_EL0, which the kernel never uses; the current EL with SP_EL1; a lower EL in AArch64; in
// AArch32, which the kernel does not run) and what it is (synchronous, IRQ, FIQ, SError).
//
// `save_frame` stores the registers it uses as scratch before it uses them, and `orig_x0` is x0
// as it was. `trap_return` first makes the page-table changes visible to the table walker (the
// `dsb`), then restores the frame and returns from the exception. It is also the first "return
// address" of every new thread (see `context::init`).
global_asm!(
    r#"
.macro save_frame
    sub sp, sp, #{frame_size}
    stp x0, x1, [sp, #16 * 0]
    stp x2, x3, [sp, #16 * 1]
    stp x4, x5, [sp, #16 * 2]
    stp x6, x7, [sp, #16 * 3]
    stp x8, x9, [sp, #16 * 4]
    stp x10, x11, [sp, #16 * 5]
    stp x12, x13, [sp, #16 * 6]
    stp x14, x15, [sp, #16 * 7]
    stp x16, x17, [sp, #16 * 8]
    stp x18, x19, [sp, #16 * 9]
    stp x20, x21, [sp, #16 * 10]
    stp x22, x23, [sp, #16 * 11]
    stp x24, x25, [sp, #16 * 12]
    stp x26, x27, [sp, #16 * 13]
    stp x28, x29, [sp, #16 * 14]
    mrs x21, sp_el0
    stp x30, x21, [sp, #16 * 15]
    mrs x22, elr_el1
    mrs x23, spsr_el1
    stp x22, x23, [sp, #16 * 16]
    stp x0, xzr, [sp, #16 * 17]
    add x24, sp, #{fp_offset}
    stp q0, q1, [x24, #32 * 0]
    stp q2, q3, [x24, #32 * 1]
    stp q4, q5, [x24, #32 * 2]
    stp q6, q7, [x24, #32 * 3]
    stp q8, q9, [x24, #32 * 4]
    stp q10, q11, [x24, #32 * 5]
    stp q12, q13, [x24, #32 * 6]
    stp q14, q15, [x24, #32 * 7]
    stp q16, q17, [x24, #32 * 8]
    stp q18, q19, [x24, #32 * 9]
    stp q20, q21, [x24, #32 * 10]
    stp q22, q23, [x24, #32 * 11]
    stp q24, q25, [x24, #32 * 12]
    stp q26, q27, [x24, #32 * 13]
    stp q28, q29, [x24, #32 * 14]
    stp q30, q31, [x24, #32 * 15]
    mrs x25, fpcr
    mrs x26, fpsr
    str x25, [x24, #512]
    str x26, [x24, #520]
.endm

.pushsection .text.vectors, "ax"
.balign 2048
.global exception_vectors
exception_vectors:
    // The current EL, with SP_EL0: never.
    .balign 128
    b unexpected_el1
    .balign 128
    b unexpected_el1
    .balign 128
    b unexpected_el1
    .balign 128
    b unexpected_el1
    // The current EL, with SP_EL1: the kernel.
    .balign 128
    b el1_sync
    .balign 128
    b el1_irq
    .balign 128
    b unexpected_el1
    .balign 128
    b unexpected_el1
    // A lower EL in AArch64: user mode.
    .balign 128
    b el0_sync
    .balign 128
    b el0_irq
    .balign 128
    b unexpected_el0
    .balign 128
    b unexpected_el0
    // A lower EL in AArch32: never.
    .balign 128
    b unexpected_el0
    .balign 128
    b unexpected_el0
    .balign 128
    b unexpected_el0
    .balign 128
    b unexpected_el0

el1_sync:
    save_frame
    mov x0, sp
    bl {el1_sync}
    b .
el1_irq:
    save_frame
    mov x0, sp
    bl {irq}
    b trap_return
el0_sync:
    save_frame
    mov x0, sp
    bl {el0_sync}
    b trap_return
el0_irq:
    save_frame
    mov x0, sp
    bl {irq}
    b trap_return
unexpected_el1:
    save_frame
    mov x0, sp
    mov x1, #0
    bl {unexpected}
    b .
unexpected_el0:
    save_frame
    mov x0, sp
    mov x1, #1
    bl {unexpected}
    b .

.global trap_return
trap_return:
    dsb ish
    add x24, sp, #{fp_offset}
    ldr x25, [x24, #512]
    ldr x26, [x24, #520]
    msr fpcr, x25
    msr fpsr, x26
    ldp q0, q1, [x24, #32 * 0]
    ldp q2, q3, [x24, #32 * 1]
    ldp q4, q5, [x24, #32 * 2]
    ldp q6, q7, [x24, #32 * 3]
    ldp q8, q9, [x24, #32 * 4]
    ldp q10, q11, [x24, #32 * 5]
    ldp q12, q13, [x24, #32 * 6]
    ldp q14, q15, [x24, #32 * 7]
    ldp q16, q17, [x24, #32 * 8]
    ldp q18, q19, [x24, #32 * 9]
    ldp q20, q21, [x24, #32 * 10]
    ldp q22, q23, [x24, #32 * 11]
    ldp q24, q25, [x24, #32 * 12]
    ldp q26, q27, [x24, #32 * 13]
    ldp q28, q29, [x24, #32 * 14]
    ldp q30, q31, [x24, #32 * 15]
    ldp x22, x23, [sp, #16 * 16]
    msr elr_el1, x22
    msr spsr_el1, x23
    ldp x30, x21, [sp, #16 * 15]
    msr sp_el0, x21
    ldp x0, x1, [sp, #16 * 0]
    ldp x2, x3, [sp, #16 * 1]
    ldp x4, x5, [sp, #16 * 2]
    ldp x6, x7, [sp, #16 * 3]
    ldp x8, x9, [sp, #16 * 4]
    ldp x10, x11, [sp, #16 * 5]
    ldp x12, x13, [sp, #16 * 6]
    ldp x14, x15, [sp, #16 * 7]
    ldp x16, x17, [sp, #16 * 8]
    ldp x18, x19, [sp, #16 * 9]
    ldp x20, x21, [sp, #16 * 10]
    ldp x22, x23, [sp, #16 * 11]
    ldp x24, x25, [sp, #16 * 12]
    ldp x26, x27, [sp, #16 * 13]
    ldp x28, x29, [sp, #16 * 14]
    add sp, sp, #{frame_size}
    eret
.popsection
"#,
    frame_size = const FRAME_SIZE,
    fp_offset = const FP_OFFSET,
    el1_sync = sym el1_sync,
    el0_sync = sym el0_sync,
    irq = sym irq,
    unexpected = sym unexpected,
);

/// A synchronous exception from user mode: a system call, or a fault.
extern "C" fn el0_sync(f: &mut TrapFrame) {
    let (esr, far) = cpu::fault_syndrome();
    let class = esr >> 26;
    if class == ec::SVC64 {
        crate::syscall::syscall_dispatch(f);
        return;
    }
    // One message, so that nothing cuts into it.
    let mut log = crate::serial::lock();
    let _ = writeln!(
        log,
        "\n[user] #{:#04x} ({}) esr={:#x} pc={:#x} sp={:#x}",
        class,
        class_name(class),
        esr,
        f.elr,
        f.sp
    );
    if has_fault_address(class) {
        let _ = writeln!(log, "    faulting address (FAR) = {:#x}", far);
    }
    drop(log);
    // The task cannot go on. What that means for its process (SIGSEGV) is the task manager's
    // business, which `end_current` tells.
    task::end_current(Why::Fault)
}

/// A synchronous exception in the kernel: a bug.
extern "C" fn el1_sync(f: &TrapFrame) -> ! {
    let (esr, far) = cpu::fault_syndrome();
    let class = esr >> 26;
    crate::println!(
        "\n[KERNEL] #{:#04x} ({}) esr={:#x} pc={:#x} far={:#x} lr={:#x}",
        class,
        class_name(class),
        esr,
        f.elr,
        far,
        f.x[30]
    );
    panic!("unrecoverable exception in kernel mode");
}

/// An exception that the kernel never expects (an FIQ, an SError, AArch32 code).
extern "C" fn unexpected(f: &TrapFrame, from_user: u64) -> ! {
    let (esr, _) = cpu::fault_syndrome();
    panic!(
        "unexpected exception from {} mode: esr={:#x} pc={:#x}",
        if from_user != 0 { "user" } else { "kernel" },
        esr,
        f.elr
    );
}

/// An interrupt (from user mode, or while the CPU idles): takes it from the GIC, and tells the
/// generic dispatch where it came from.
extern "C" fn irq(f: &mut TrapFrame) {
    let intid = gic::acknowledge();
    let source = match intid {
        gic::TICK => {
            // The timer's interrupt stays asserted until it is armed again.
            timer::rearm();
            percpu::set_interrupt(intid);
            Interrupt::Tick
        }
        gic::RESCHEDULE => {
            percpu::set_interrupt(intid);
            Interrupt::Reschedule
        }
        // The platform's lines; its controller takes care of the end of interrupt.
        gic::SPI_FIRST..gic::SPECIAL => Interrupt::Line((intid - gic::SPI_FIRST) as usize),
        gic::SPECIAL.. => Interrupt::Spurious,
        // An interrupt of the CPU's that the kernel does not use.
        _ => {
            gic::end_of_interrupt(Some(intid));
            Interrupt::Spurious
        }
    };
    crate::irq::dispatch(source, f);
}
