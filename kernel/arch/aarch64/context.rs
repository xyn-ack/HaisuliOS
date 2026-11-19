// SPDX-License-Identifier: MIT

//! Kernel contexts.
//!
//! Kernel stacks and context switching: one context per thread slot, plus one idle context per
//! CPU.
//!
//! A suspended context is a saved kernel stack pointer. Only this module writes them: [`init`]
//! builds a fresh stack that returns to user mode, and [`switch_to`] saves the running context
//! before resuming another. A thread's context may be resumed on any CPU, so the module tracks
//! which contexts are *on a CPU*: from the moment a CPU switches to one until another switch has
//! saved it and left its stack. Nothing may resume a context, or build a new one in its slot,
//! while it is on a CPU; [`switch_to`] and [`init`] wait for that. That, and refusing to resume
//! contexts that were never saved, is why the interface is safe.
//!
//! An idle context is the stack a CPU booted on: it is "saved" the first time the CPU switches
//! away from it, so it needs no [`init`].

use super::entry::TrapFrame;
use super::percpu::{self, MAX_CPUS};
use crate::task::MAX_THREADS;
use core::arch::global_asm;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const KSTACK_SIZE: usize = 16 * 1024;
const KSTACK_WORDS: usize = KSTACK_SIZE / 8;
/// A suspended context on its stack, as `switch_context` saves it: x19 to x28, the frame pointer
/// x29, the link register x30 (where `switch_context` returns to), then d8 to d15 (the low
/// halves of v8 to v15, which the procedure call standard also makes callee-saved).
const SWITCH_FRAME_WORDS: usize = 20;
/// Where x30 is in the switch frame.
const RETURN_ADDRESS: usize = 11;
/// Thread contexts, then idle contexts.
const CONTEXTS: usize = MAX_THREADS + MAX_CPUS;

/// Kernel stack of one thread slot.
///
/// Atomics give interior mutability: Rust code writes a new stack's initial
/// frames, and the CPU uses it as a stack.
#[repr(C, align(16))]
struct KernelStack([AtomicU64; KSTACK_WORDS]);

static STACKS: [KernelStack; MAX_THREADS] =
    [const { KernelStack([const { AtomicU64::new(0) }; KSTACK_WORDS]) }; MAX_THREADS];
/// Saved stack pointer of each suspended context.
static SAVED_SP: [AtomicU64; CONTEXTS] = [const { AtomicU64::new(0) }; CONTEXTS];
/// Whether `SAVED_SP[context]` holds a context that can be resumed.
static RESUMABLE: [AtomicBool; CONTEXTS] = [const { AtomicBool::new(false) }; CONTEXTS];
/// Whether a CPU runs on the context's stack (or has not finished leaving it).
static ON_CPU: [AtomicBool; CONTEXTS] = [const { AtomicBool::new(false) }; CONTEXTS];
/// The context each CPU runs.
static RUNNING: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(NONE) }; MAX_CPUS];
const NONE: usize = usize::MAX;

extern "C" {
    /// Save callee-saved registers and sp to `*save`, load `new_sp`, then clear `*on_cpu`
    /// (the saved context's flag) and restore.
    fn switch_context(save: *mut u64, new_sp: u64, on_cpu: *const AtomicBool);
    fn trap_return();
}

// A new thread's stack is crafted so the final `ret` lands in `trap_return`,
// which restores a TrapFrame and `eret`s to EL0.
//
// The old context's `on_cpu` flag is cleared only once its stack pointer is
// saved and this CPU runs on the new stack: from then on another CPU may
// resume it. The store-release makes the saved sp visible first.
global_asm!(
    r#"
.pushsection .text
.global switch_context
switch_context:
    sub sp, sp, #{frame}
    stp x19, x20, [sp, #16 * 0]
    stp x21, x22, [sp, #16 * 1]
    stp x23, x24, [sp, #16 * 2]
    stp x25, x26, [sp, #16 * 3]
    stp x27, x28, [sp, #16 * 4]
    stp x29, x30, [sp, #16 * 5]
    stp d8, d9, [sp, #16 * 6]
    stp d10, d11, [sp, #16 * 7]
    stp d12, d13, [sp, #16 * 8]
    stp d14, d15, [sp, #16 * 9]
    mov x9, sp
    str x9, [x0]
    mov sp, x1
    stlrb wzr, [x2]
    ldp x19, x20, [sp, #16 * 0]
    ldp x21, x22, [sp, #16 * 1]
    ldp x23, x24, [sp, #16 * 2]
    ldp x25, x26, [sp, #16 * 3]
    ldp x27, x28, [sp, #16 * 4]
    ldp x29, x30, [sp, #16 * 5]
    ldp d8, d9, [sp, #16 * 6]
    ldp d10, d11, [sp, #16 * 7]
    ldp d12, d13, [sp, #16 * 8]
    ldp d14, d15, [sp, #16 * 9]
    add sp, sp, #{frame}
    ret
.popsection
"#,
    frame = const SWITCH_FRAME_WORDS * 8,
);

/// The context of thread slot `slot`.
pub(crate) fn thread(slot: usize) -> usize {
    assert!(slot < MAX_THREADS);
    slot
}

/// The idle context of CPU `cpu`.
pub(crate) fn idle(cpu: usize) -> usize {
    MAX_THREADS + cpu
}

/// Makes the code running now (on a CPU's boot stack) this CPU's idle context.
pub(crate) fn enter_idle() {
    let cpu = percpu::index();
    let ctx = idle(cpu);
    assert!(
        RUNNING[cpu].load(Ordering::Relaxed) == NONE,
        "CPU {} idles already",
        cpu
    );
    ON_CPU[ctx].store(true, Ordering::Relaxed);
    RUNNING[cpu].store(ctx, Ordering::Relaxed);
}

/// Whether a CPU still runs on (or has not finished leaving) thread slot `slot`'s stack.
pub(crate) fn on_cpu(slot: usize) -> bool {
    ON_CPU[thread(slot)].load(Ordering::Acquire)
}

/// The user registers of thread slot `slot`, which is in a system call.
///
/// The exception vector saves them in a [`TrapFrame`] at the very top of the kernel
/// stack, and they stay there until the call returns, so they can be read
/// even while the thread is still on its way off its CPU. (If it is not in a
/// system call, this returns whatever is there: it reads only atomics.)
pub(crate) fn user_frame(slot: usize) -> TrapFrame {
    let stack = &STACKS[slot].0;
    let mut words = [0; TrapFrame::WORDS];
    for (w, saved) in words
        .iter_mut()
        .zip(&stack[KSTACK_WORDS - TrapFrame::WORDS..])
    {
        *w = saved.load(Ordering::Relaxed);
    }
    TrapFrame::from_words(&words)
}

/// Prepares thread slot `slot` to enter user mode with the registers in `frame` the next time
/// it is switched to.
///
/// Waits while a CPU is still leaving the slot's old context: so the caller must not hold a lock
/// that the leaving CPU needs in order to switch away (the scheduler's). A slot that no thread
/// runs in, or one whose thread [`on_cpu`] has seen leave and which cannot run meanwhile, is
/// ready at once.
///
/// # Panics
///
/// Panics if this CPU runs the slot's context.
pub(crate) fn init(slot: usize, frame: &TrapFrame) {
    let ctx = thread(slot);
    assert!(
        RUNNING[percpu::index()].load(Ordering::Relaxed) != ctx,
        "cannot reinitialise the running context"
    );
    while ON_CPU[ctx].load(Ordering::Acquire) {
        spin_loop();
    }
    let stack = &STACKS[slot].0;
    // [x19 .. x29, x30 = trap_return, d8 .. d15][TrapFrame] <- top
    let frame_words = frame.words();
    let first = KSTACK_WORDS - TrapFrame::WORDS - SWITCH_FRAME_WORDS;
    for w in &stack[first..first + SWITCH_FRAME_WORDS] {
        w.store(0, Ordering::Relaxed);
    }
    stack[first + RETURN_ADDRESS].store(trap_return as *const () as u64, Ordering::Relaxed);
    for (w, &v) in stack[first + SWITCH_FRAME_WORDS..]
        .iter()
        .zip(frame_words.iter())
    {
        w.store(v, Ordering::Relaxed);
    }
    SAVED_SP[ctx].store(stack[first..].as_ptr() as u64, Ordering::Relaxed);
    RESUMABLE[ctx].store(true, Ordering::Release);
}

/// Suspends the context running on this CPU and resumes context `next`.
///
/// Returns when some CPU switches back to the suspended context. Waits while `next` is still on
/// its previous CPU.
///
/// # Panics
///
/// Panics if `next` has no context to resume, or if this CPU has no context yet
/// ([`enter_idle`]).
pub(crate) fn switch_to(next: usize) {
    let cpu = percpu::index();
    let prev = RUNNING[cpu].load(Ordering::Relaxed);
    assert!(prev != NONE, "CPU {} has no context", cpu);
    if prev == next {
        return;
    }
    while ON_CPU[next].load(Ordering::Acquire) {
        spin_loop();
    }
    let is_idle = next >= MAX_THREADS;
    assert!(
        is_idle || RESUMABLE[next].swap(false, Ordering::Acquire),
        "context {} has no context to resume",
        next
    );
    ON_CPU[next].store(true, Ordering::Relaxed);
    RUNNING[cpu].store(next, Ordering::Relaxed);
    if prev < MAX_THREADS {
        RESUMABLE[prev].store(true, Ordering::Relaxed);
    }
    // SAFETY: `SAVED_SP[next]` was written by `init` or by the `switch_context` that suspended
    // `next` (it was resumable and no longer on any CPU, and only this module writes these), so
    // it is a valid suspended stack. `SAVED_SP[prev]` and `ON_CPU[prev]` are atomics that
    // nothing else writes while `prev` is on this CPU.
    unsafe {
        switch_context(
            SAVED_SP[prev].as_ptr(),
            SAVED_SP[next].load(Ordering::Relaxed),
            &ON_CPU[prev],
        )
    };
}
