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
/// Registers that `switch_context` saves on the stack: rbp, rbx and r12 to r15.
const CALLEE_SAVED: usize = 6;
/// A suspended context on its stack: the callee-saved registers, then the
/// address `switch_context` returns to.
const SWITCH_FRAME_WORDS: usize = CALLEE_SAVED + 1;
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
static SAVED_RSP: [AtomicU64; CONTEXTS] = [const { AtomicU64::new(0) }; CONTEXTS];
/// Whether `SAVED_RSP[context]` holds a context that can be resumed.
static RESUMABLE: [AtomicBool; CONTEXTS] = [const { AtomicBool::new(false) }; CONTEXTS];
/// Whether a CPU runs on the context's stack (or has not finished leaving it).
static ON_CPU: [AtomicBool; CONTEXTS] = [const { AtomicBool::new(false) }; CONTEXTS];
/// The context each CPU runs.
static RUNNING: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(NONE) }; MAX_CPUS];
const NONE: usize = usize::MAX;

extern "C" {
    /// Save callee-saved registers and rsp to `*save`, load `new_rsp`, then clear `*on_cpu`
    /// (the saved context's flag) and restore.
    fn switch_context(save: *mut u64, new_rsp: u64, on_cpu: *const AtomicBool);
    fn trap_return();
}

// A new thread's stack is crafted so the final `ret` lands in `trap_return`,
// which pops a TrapFrame and iretq's to ring 3.
//
// The old context's `on_cpu` flag is cleared only once its stack pointer is
// saved and this CPU runs on the new stack: from then on another CPU may
// resume it. (x86 keeps stores in order, so the saved rsp is visible first.)
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
    mov byte ptr [rdx], 0
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

/// Top of `slot`'s kernel stack (where the CPU puts a user-mode trap frame).
pub(crate) fn stack_top(slot: usize) -> u64 {
    STACKS[slot].0.as_ptr() as u64 + KSTACK_SIZE as u64
}

/// Whether a CPU still runs on (or has not finished leaving) thread slot `slot`'s stack.
pub(crate) fn on_cpu(slot: usize) -> bool {
    ON_CPU[thread(slot)].load(Ordering::Acquire)
}

/// The user registers of thread slot `slot`, which is in a system call.
///
/// `syscall_entry` saves them in a [`TrapFrame`] at the very top of the kernel
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
    // [r15 r14 r13 r12 rbx rbp][return address = trap_return][TrapFrame] <- top
    let frame_words = frame.words();
    let first = KSTACK_WORDS - TrapFrame::WORDS - SWITCH_FRAME_WORDS;
    for w in &stack[first..first + CALLEE_SAVED] {
        w.store(0, Ordering::Relaxed);
    }
    stack[first + CALLEE_SAVED].store(trap_return as *const () as u64, Ordering::Relaxed);
    for (w, &v) in stack[first + SWITCH_FRAME_WORDS..]
        .iter()
        .zip(frame_words.iter())
    {
        w.store(v, Ordering::Relaxed);
    }
    SAVED_RSP[ctx].store(stack[first..].as_ptr() as u64, Ordering::Relaxed);
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
    // SAFETY: `SAVED_RSP[next]` was written by `init` or by the `switch_context` that suspended
    // `next` (it was resumable and no longer on any CPU, and only this module writes these), so
    // it is a valid suspended stack. `SAVED_RSP[prev]` and `ON_CPU[prev]` are atomics that
    // nothing else writes while `prev` is on this CPU.
    unsafe {
        switch_context(
            SAVED_RSP[prev].as_ptr(),
            SAVED_RSP[next].load(Ordering::Relaxed),
            &ON_CPU[prev],
        )
    };
}
