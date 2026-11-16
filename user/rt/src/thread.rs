// SPDX-License-Identifier: GPL-2.0

//! Threads.
//!
//! More threads of execution in the calling process: they share its memory, its file
//! descriptors, its pid and its signal dispositions.
//!
//! The kernel provides the mechanism, with Linux's calls: `clone` starts a thread in the caller's
//! task ([`crate::abi::clone::THREAD_FLAGS`]), `exit` ends one ([`sys::thread_exit`]), `gettid`
//! names it ([`sys::gettid`]). The process server keeps the POSIX view: whichever thread asks,
//! `getpid` gives the process's pid, `exit` ends the whole process with all its threads, `fork`
//! copies only the thread that calls it and `execve` ends the others. This module is what a
//! libc's `pthread_create` and `pthread_join` do on top of that, in the style of the rest of the
//! runtime:
//!
//! ```ignore
//! extern "C" fn work(arg: u64) { /* ... */ }
//! let t = thread::spawn(work, 42)?;
//! t.join();
//! ```
//!
//! A thread runs a function `extern "C" fn(u64)`; when that returns, the thread ends. Every
//! thread gets a stack of [`STACK_SIZE`] bytes, mapped the first time its slot is used and
//! reused by the threads after it (up to [`MAX_THREADS`] at a time).

use crate::abi::clone::THREAD_FLAGS;
use crate::abi::errno::EAGAIN;
use crate::abi::mman::{PROT_READ, PROT_WRITE};
use crate::abi::nr;
use crate::sys::{self, Errno, Result};
use core::arch::global_asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The most threads a process can start with [`spawn`] that have not been joined yet.
pub const MAX_THREADS: usize = 64;
/// Bytes of stack each thread gets.
pub const STACK_SIZE: u64 = 64 * 1024;

/// What a thread runs: `job(arg)`, after which the thread ends.
pub type Job = extern "C" fn(u64);

/// The state of each thread slot (and its stack): [`FREE`], [`RUNNING`] or [`FINISHED`].
static STATE: [AtomicU32; MAX_THREADS] = [const { AtomicU32::new(FREE) }; MAX_THREADS];
/// Each slot's stack: its lowest address, once mapped (0 before).
static STACK: [AtomicU64; MAX_THREADS] = [const { AtomicU64::new(0) }; MAX_THREADS];
const FREE: u32 = 0;
const RUNNING: u32 = 1;
/// Stored by the ending thread itself once it no longer touches its stack (see
/// `__haisuli_thread_exit`), so that [`Thread::join`] may hand the stack out again.
const FINISHED: u32 = 2;

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

    /// Stores `finished` into `*state` and ends the calling thread.
    fn __haisuli_thread_exit(state: *const AtomicU32, finished: u32) -> !;
}

// `__haisuli_clone` leaves what the new thread needs on its stack: after `clone`, the new thread
// has no other memory of its own and only `rsp` tells it apart from its creator. Its frame is
// [start][slot][job][arg] from `stack_top - 32` up; popping it leaves `rsp` 16-byte aligned, as
// the System V ABI wants it before a `call`.
//
// `__haisuli_thread_exit` writes nothing to the stack after marking it finished: from then on,
// the stack may be handed to the next thread while this one is still on its way out.
global_asm!(
    ".pushsection .text.__haisuli_clone, \"ax\"",
    ".global __haisuli_clone",
    "__haisuli_clone:",
    "    sub rsi, 32",
    "    mov [rsi], rdx",
    "    mov [rsi + 8], rcx",
    "    mov [rsi + 16], r8",
    "    mov [rsi + 24], r9",
    "    mov eax, {clone}",
    "    xor edx, edx",
    "    xor r10d, r10d",
    "    xor r8d, r8d",
    "    syscall",
    "    test rax, rax",
    "    jz 2f",
    "    ret",
    "2:",
    "    pop rax",
    "    pop rdi",
    "    pop rsi",
    "    pop rdx",
    "    xor ebp, ebp",
    "    call rax",
    "    ud2",
    ".global __haisuli_thread_exit",
    "__haisuli_thread_exit:",
    "    mov [rdi], esi",
    "    mov eax, {exit}",
    "    xor edi, edi",
    "    syscall",
    "    ud2",
    ".popsection",
    clone = const nr::CLONE,
    exit = const nr::EXIT,
);

/// The first Rust code of a new thread.
extern "C" fn thread_start(slot: u64, job: Job, arg: u64) -> ! {
    job(arg);
    // SAFETY: `slot` is the slot that `spawn` reserved for this thread, so `STATE[slot]` is a
    // live atomic; the thread ends inside the call and its stack is not used again.
    unsafe { __haisuli_thread_exit(&STATE[slot as usize], FINISHED) }
}

/// A thread started by [`spawn`].
pub struct Thread {
    slot: usize,
    /// Its thread id, as `clone` returned it.
    tid: u64,
}

impl Thread {
    /// The thread's id (what [`sys::gettid`] returns in it).
    pub fn id(&self) -> u64 {
        self.tid
    }

    /// Waits until the thread has ended, letting the other threads run meanwhile.
    pub fn join(self) {
        while STATE[self.slot].load(Ordering::Acquire) != FINISHED {
            sys::sched_yield();
        }
        STATE[self.slot].store(FREE, Ordering::Release);
    }
}

/// The stack of `slot`: its top. Mapped the first time the slot is used.
fn stack_top(slot: usize) -> Result<u64> {
    let mut base = STACK[slot].load(Ordering::Acquire);
    if base == 0 {
        base = sys::mmap(STACK_SIZE, PROT_READ | PROT_WRITE)?;
        STACK[slot].store(base, Ordering::Release);
    }
    Ok(base + STACK_SIZE)
}

/// Starts a thread that runs `job(arg)`.
///
/// Fails with `EAGAIN` if [`MAX_THREADS`] threads have not been joined yet or the kernel has no
/// room for another thread, and with `ENOMEM` if there is no memory for its stack.
pub fn spawn(job: Job, arg: u64) -> Result<Thread> {
    let slot = STATE
        .iter()
        .position(|s| {
            s.compare_exchange(FREE, RUNNING, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        })
        .ok_or(Errno(EAGAIN))?;
    let stack_top = match stack_top(slot) {
        Ok(top) => top,
        Err(e) => {
            STATE[slot].store(FREE, Ordering::Release);
            return Err(e);
        }
    };
    // SAFETY: The slot is reserved for this thread, so nothing else uses its stack, which is
    // mapped memory that no Rust object lives in; the new thread only runs `thread_start`,
    // which takes everything it uses from the frame that `__haisuli_clone` writes. The flags
    // share the address space, so the statics it uses are the ones this thread sees.
    let tid =
        unsafe { __haisuli_clone(THREAD_FLAGS, stack_top, thread_start, slot as u64, job, arg) };
    if tid <= 0 {
        STATE[slot].store(FREE, Ordering::Release);
        return Err(Errno(if tid == 0 { EAGAIN } else { -tid }));
    }
    Ok(Thread {
        slot,
        tid: tid as u64,
    })
}
