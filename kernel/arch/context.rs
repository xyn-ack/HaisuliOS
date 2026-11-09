// SPDX-License-Identifier: GPL-2.0

//! Kernel contexts.
//!
//! Kernel stacks and context switching, with one context per process slot.
//!
//! A suspended context is a saved kernel stack pointer. Only this module writes them: [`init`]
//! builds a fresh stack that returns to user mode, and [`switch_to`] saves the running context
//! before resuming another. It also tracks which context is running and which ones can be resumed,
//! so it can refuse to resume garbage or to overwrite the stack it is running on. That is why its
//! interface is safe.

use super::entry::TrapFrame;
use crate::process::MAX_PROCS;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const KSTACK_SIZE: usize = 16 * 1024;
const KSTACK_WORDS: usize = KSTACK_SIZE / 8;

/// Kernel stack of one process slot.
///
/// Atomics give interior mutability: Rust code writes a new stack's initial
/// frames, and the CPU uses it as a stack.
#[repr(C, align(16))]
struct KernelStack([AtomicU64; KSTACK_WORDS]);

static STACKS: [KernelStack; MAX_PROCS] =
    [const { KernelStack([const { AtomicU64::new(0) }; KSTACK_WORDS]) }; MAX_PROCS];
/// Saved stack pointer of each suspended context.
static SAVED_RSP: [AtomicU64; MAX_PROCS] = [const { AtomicU64::new(0) }; MAX_PROCS];
/// Whether `SAVED_RSP[slot]` holds a context that can be resumed.
static RESUMABLE: [AtomicBool; MAX_PROCS] = [const { AtomicBool::new(false) }; MAX_PROCS];
/// Slot whose context is running; NONE while still on the boot stack.
static RUNNING: AtomicUsize = AtomicUsize::new(NONE);
const NONE: usize = usize::MAX;

extern "C" {
    /// Save callee-saved registers and rsp to `*save`, load `new_rsp`, restore.
    fn switch_context(save: *mut u64, new_rsp: u64);
    fn trap_return();
}

// A new process's stack is crafted so the final `ret` lands in
// `trap_return`, which pops a TrapFrame and iretq's to ring 3.
global_asm!(
    r#"
.pushsection .text
.global switch_context
switch_context:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
.popsection
"#
);

/// Top of `slot`'s kernel stack (where the CPU puts a user-mode trap frame).
pub(crate) fn stack_top(slot: usize) -> u64 {
    STACKS[slot].0.as_ptr() as u64 + KSTACK_SIZE as u64
}

/// Prepares `slot` to enter user mode with the registers in `frame` the next
/// time it is switched to.
///
/// # Panics
///
/// Panics if `slot` is the running context.
pub(crate) fn init(slot: usize, frame: &TrapFrame) {
    assert!(
        RUNNING.load(Ordering::Relaxed) != slot,
        "cannot reinitialise the running context"
    );
    let stack = &STACKS[slot].0;
    // [r15 r14 r13 r12 rbx rbp][return address = trap_return][TrapFrame] <- top
    let frame_words = frame.words();
    let first = KSTACK_WORDS - TrapFrame::WORDS - 7;
    for w in &stack[first..first + 6] {
        w.store(0, Ordering::Relaxed);
    }
    stack[first + 6].store(trap_return as *const () as u64, Ordering::Relaxed);
    for (w, &v) in stack[first + 7..].iter().zip(frame_words.iter()) {
        w.store(v, Ordering::Relaxed);
    }
    SAVED_RSP[slot].store(stack[first..].as_ptr() as u64, Ordering::Relaxed);
    RESUMABLE[slot].store(true, Ordering::Relaxed);
}

/// Suspends the running context and resumes `slot`'s.
///
/// Returns when some other context switches back to this one.
///
/// # Panics
///
/// Panics if `slot` has no context to resume.
pub(crate) fn switch_to(slot: usize) {
    let old = RUNNING.load(Ordering::Relaxed);
    if old == slot {
        return;
    }
    assert!(
        RESUMABLE[slot].swap(false, Ordering::Relaxed),
        "slot {} has no context to resume",
        slot
    );
    RUNNING.store(slot, Ordering::Relaxed);
    let save = if old == NONE {
        // Leaving the boot stack for good; its saved state is never used.
        static BOOT_RSP: AtomicU64 = AtomicU64::new(0);
        BOOT_RSP.as_ptr()
    } else {
        RESUMABLE[old].store(true, Ordering::Relaxed);
        SAVED_RSP[old].as_ptr()
    };
    // SAFETY: `SAVED_RSP[slot]` was written by `init` or by the
    // `switch_context` that suspended `slot` (it was RESUMABLE, and only this
    // module writes these), so it is a valid suspended stack; `save` points
    // to an atomic that nothing else accesses while the CPU writes it.
    unsafe { switch_context(save, SAVED_RSP[slot].load(Ordering::Relaxed)) };
}
