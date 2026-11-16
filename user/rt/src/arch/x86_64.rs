// SPDX-License-Identifier: GPL-2.0

//! x86_64: the `syscall` instruction, the program and thread entry points, the signal restorer,
//! and port I/O.
//!
//! The system call convention is Linux's: the number in `rax`, the arguments in
//! `rdi rsi rdx r10 r8 r9`, the result in `rax`; `syscall` itself clobbers `rcx` and `r11`.

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
    // SAFETY: The caller guarantees the arguments are valid for call `n`.
    // The kernel preserves every register except rax; rcx and r11 are
    // clobbered by the `syscall` instruction itself.
    unsafe {
        asm!("syscall",
             inlateout("rax") n as i64 => ret,
             in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, in("r8") a4, in("r9") a5,
             lateout("rcx") _, lateout("r11") _,
             options(nostack));
    }
    ret
}

/// The program's `_start`, for [`crate::entry!`]: calls [`crate::__start`] with `main` and the
/// initial stack pointer.
///
/// The kernel starts a program with an aligned stack and no return address; this sets up a
/// proper System V call frame.
#[doc(hidden)]
#[macro_export]
macro_rules! __haisuli_start {
    ($main:path) => {
        core::arch::global_asm!(
            ".pushsection .text._start, \"ax\"",
            ".global _start",
            "_start:",
            "    xor ebp, ebp",
            "    mov rsi, rsp",
            "    and rsp, -16",
            "    lea rdi, [rip + {main}]",
            "    call {start}",
            "    ud2",
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
// has no other memory of its own and only `rsp` tells it apart from its creator. Its frame is
// [start][slot][job][arg] from `stack_top - 32` up; popping it leaves `rsp` 16-byte aligned, as
// the System V ABI wants it before a `call`.
//
// `__haisuli_thread_exit` writes nothing to the stack after marking it finished: from then on,
// the stack may be handed to the next thread while this one is still on its way out. (x86's
// stores have release ordering.)
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

// Where every signal handler returns to: the kernel's upcall frame has this
// address as the handler's return address, and the stack pointer then points
// at the frame's `ucontext` (see `upcall::UC_SIGMASK`). The signal mask saved
// there goes back to the process server; then `rt_sigreturn` restores the
// interrupted registers from the frame (it never returns here). The `call`
// keeps the System V alignment: the kernel leaves the stack pointer 16-byte
// aligned at this point.
global_asm!(
    ".pushsection .text.__haisuli_sigreturn, \"ax\"",
    ".global __haisuli_sigreturn",
    "__haisuli_sigreturn:",
    "    mov rdi, [rsp + {uc_sigmask}]",
    "    call {restore}",
    "    mov eax, {nr}",
    "    syscall",
    "    ud2",
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

/// Port I/O, for the devices granted to the program ([`crate::device`]).
pub(crate) mod ports {
    use core::arch::asm;

    /// Reads the byte register at I/O port `port`.
    ///
    /// # Safety
    ///
    /// `port` must belong to a device that the kernel granted the program and that cannot
    /// access memory (the kernel grants only such devices' ports).
    pub(crate) unsafe fn read8(port: u16) -> u8 {
        let v: u8;
        // SAFETY: Port I/O on such a device touches no memory.
        unsafe {
            asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags))
        };
        v
    }

    /// Writes the byte register at I/O port `port`.
    ///
    /// # Safety
    ///
    /// As for [`read8`].
    pub(crate) unsafe fn write8(port: u16, v: u8) {
        // SAFETY: As in `read8`.
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags))
        };
    }

    /// Reads the 32-bit register at I/O port `port`.
    ///
    /// # Safety
    ///
    /// As for [`read8`].
    pub(crate) unsafe fn read32(port: u16) -> u32 {
        let v: u32;
        // SAFETY: As in `read8`.
        unsafe {
            asm!(
                "in eax, dx",
                out("eax") v,
                in("dx") port,
                options(nomem, nostack, preserves_flags),
            )
        };
        v
    }

    /// Writes the 32-bit register at I/O port `port`.
    ///
    /// # Safety
    ///
    /// As for [`read8`].
    pub(crate) unsafe fn write32(port: u16, v: u32) {
        // SAFETY: As in `read8`.
        unsafe {
            asm!(
                "out dx, eax",
                in("dx") port,
                in("eax") v,
                options(nomem, nostack, preserves_flags),
            )
        };
    }
}
