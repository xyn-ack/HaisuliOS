// SPDX-License-Identifier: GPL-2.0

//! arm64: the `svc` instruction, the program and thread entry points, and the signal restorer.
//!
//! The system call convention is Linux's: the number in `x8`, the arguments in `x0` to `x5`, the
//! result in `x0`; every other register is preserved. arm64 has no I/O ports: its devices are
//! memory-mapped only.

use crate::abi::{nr, upcall};
use crate::thread::Job;
use core::arch::{asm, global_asm};
use core::sync::atomic::AtomicU32;

/// Makes system call `n` with arguments `a0` to `a5`.
///
/// # Safety
///
/// As for [`crate::sys::syscall`].
#[inline(always)]
pub(crate) unsafe fn syscall(n: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    // SAFETY: The caller guarantees the arguments are valid for call `n`. The kernel preserves
    // every register except x0, which returns the result.
    unsafe {
        asm!("svc #0",
             inlateout("x0") a0 as i64 => ret,
             in("x1") a1, in("x2") a2, in("x3") a3, in("x4") a4, in("x5") a5,
             in("x8") n,
             options(nostack));
    }
    ret
}

/// The program's `_start`, for [`crate::entry!`]: calls [`crate::__start`] with `main` and the
/// initial stack pointer.
///
/// The kernel starts a program with the stack pointer at `argc`; this clears the frame pointer
/// and the link register (the end of the frame chain) and aligns the stack for the call.
#[doc(hidden)]
#[macro_export]
macro_rules! __haisuli_start {
    ($main:path) => {
        core::arch::global_asm!(
            ".pushsection .text._start, \"ax\"",
            ".global _start",
            "_start:",
            "    mov x29, xzr",
            "    mov x30, xzr",
            "    mov x1, sp",
            "    mov x9, sp",
            "    and x9, x9, #0xfffffffffffffff0",
            "    mov sp, x9",
            "    adrp x0, {main}",
            "    add x0, x0, :lo12:{main}",
            "    bl {start}",
            "    brk #0",
            ".popsection",
            main = sym $main,
            start = sym $crate::__start,
        );
    };
}

extern "C" {
    /// Starts a thread on the stack below `stack_top` that calls `start(slot, job, arg)`.
    ///
    /// Returns its thread id, or a negative errno.
    fn __haisuli_clone(
        flags: u64,
        stack_top: u64,
        start: extern "C" fn(u64, Job, u64) -> !,
        slot: u64,
        job: Job,
        arg: u64,
    ) -> i64;

    /// Stores `finished` into `*state` (with release ordering) and ends the calling thread.
    fn __haisuli_thread_exit(state: *const AtomicU32, finished: u32) -> !;

    /// The restorer of every signal handler (only its address is used; it is never called from
    /// Rust).
    fn __haisuli_sigreturn();
}

// `__haisuli_clone` leaves what the new thread needs on its stack: after `clone`, the new thread
// has no other memory of its own and only `sp` tells it apart from its creator. Its frame is
// [start][slot][job][arg] from `stack_top - 32` up, and the new thread starts with `sp` there;
// loading it leaves `sp` 16-byte aligned, as the procedure call standard requires. The other
// arguments of `clone` (parent tid, tls, child tid) are 0.
//
// `__haisuli_thread_exit` writes nothing to the stack after marking it finished (a store-release,
// which `join`'s load-acquire pairs with): from then on, the stack may be handed to the next
// thread while this one is still on its way out.
global_asm!(
    ".pushsection .text.__haisuli_clone, \"ax\"",
    ".global __haisuli_clone",
    "__haisuli_clone:",
    "    sub x1, x1, #32",
    "    stp x2, x3, [x1]",
    "    stp x4, x5, [x1, #16]",
    "    mov x8, #{clone}",
    "    mov x2, xzr",
    "    mov x3, xzr",
    "    mov x4, xzr",
    "    svc #0",
    "    cbz x0, 1f",
    "    ret",
    "1:",
    "    ldp x9, x0, [sp], #16",
    "    ldp x1, x2, [sp], #16",
    "    mov x29, xzr",
    "    mov x30, xzr",
    "    blr x9",
    "    brk #0",
    ".global __haisuli_thread_exit",
    "__haisuli_thread_exit:",
    "    stlr w1, [x0]",
    "    mov x8, #{exit}",
    "    mov x0, xzr",
    "    svc #0",
    "    brk #0",
    ".popsection",
    clone = const nr::CLONE,
    exit = const nr::EXIT,
);

// Where every signal handler returns to: the kernel's upcall sets the link register to the
// restorer, and the handler returns with the stack pointer where the kernel left it, at the
// frame (see `upcall::UC_SIGMASK`). The signal mask saved there goes back to the process server;
// then `rt_sigreturn` restores the interrupted registers from the frame (it never returns here).
// The frame is 16-byte aligned, as the call needs.
global_asm!(
    ".pushsection .text.__haisuli_sigreturn, \"ax\"",
    ".global __haisuli_sigreturn",
    "__haisuli_sigreturn:",
    "    ldr x0, [sp, #{uc_sigmask}]",
    "    bl {restore}",
    "    mov x8, #{nr}",
    "    svc #0",
    "    brk #0",
    ".popsection",
    uc_sigmask = const upcall::UC_SIGMASK,
    restore = sym crate::sys::restore_signal_mask,
    nr = const nr::RT_SIGRETURN,
);

/// Starts a thread on the stack below `stack_top` that calls `start(slot, job, arg)`, and returns
/// its id or a negative errno.
///
/// # Safety
///
/// The stack must be mapped memory that nothing else uses while the thread lives, and `start`
/// must not return.
pub(crate) unsafe fn clone_thread(
    flags: u64,
    stack_top: u64,
    start: extern "C" fn(u64, Job, u64) -> !,
    slot: u64,
    job: Job,
    arg: u64,
) -> i64 {
    // SAFETY: As the caller guarantees.
    unsafe { __haisuli_clone(flags, stack_top, start, slot, job, arg) }
}

/// Stores `finished` into `*state` with release ordering, without touching the stack afterwards,
/// and ends the calling thread.
///
/// # Safety
///
/// `state` must be a live atomic, and the thread's stack must not be used again.
pub(crate) unsafe fn thread_exit(state: &AtomicU32, finished: u32) -> ! {
    // SAFETY: As the caller guarantees.
    unsafe { __haisuli_thread_exit(state, finished) }
}

/// The address of the signal restorer (see `__haisuli_sigreturn` above).
pub(crate) fn sigreturn_restorer() -> u64 {
    __haisuli_sigreturn as *const () as usize as u64
}
