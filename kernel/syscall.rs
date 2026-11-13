// SPDX-License-Identifier: GPL-2.0

//! System calls.
//!
//! Dispatches the `syscall` instruction to the kernel's system call implementations.
//!
//! System calls use the Linux x86_64 register convention and, for the POSIX subset, the Linux
//! numbers, so that a future user-space POSIX personality server or libc port is easy: `rax` holds
//! the number, the arguments are in `rdi rsi rdx r10 r8 r9` and the result is returned in `rax` (a
//! negative errno on error).

use crate::arch::entry::{self, TrapFrame};
use crate::fs::{self, Transfer};
use crate::process::{self, Access};
use crate::{irq, serial, vmm};
use haisuli_abi::console::{OP_READ, OP_WRITE};
use haisuli_abi::errno::{EBADF, EFAULT, ENAMETOOLONG, ENOSYS};
use haisuli_abi::fcntl::PATH_MAX;
use haisuli_abi::{nr, Termination};

pub(crate) fn init() {
    entry::init_syscalls();
}

/// Kernel stack for the next `syscall` (the running process's).
pub(crate) fn set_kernel_stack(top: u64) {
    entry::set_syscall_stack(top);
}

/// Called by `syscall_entry` in `arch::entry` with the saved user registers;
/// the result goes back in rax.
///
/// On the way back to user mode it delivers the caller's pending signals,
/// which may redirect `f` to a signal handler.
pub(crate) extern "C" fn syscall_dispatch(f: &mut TrapFrame) {
    let nr = f.rax;
    let (a0, a1, a2, a3) = (f.rdi, f.rsi, f.rdx, f.r10);
    let ret: i64 = match nr {
        nr::READ => sys_read(a0, a1, a2),
        nr::WRITE => sys_write(a0, a1, a2),
        nr::OPEN => sys_open(a0, a1),
        nr::CLOSE => process::close(a0),
        nr::DUP => process::dup(a0, 0),
        nr::DUP2 => process::dup2(a0, a1),
        nr::FCNTL => process::fcntl(a0, a1, a2),
        nr::GETDENTS64 => sys_getdents64(a0, a1, a2),
        nr::MMAP => process::mmap(a1, a2),
        nr::SCHED_YIELD => process::yield_now(),
        nr::GETPID => process::getpid(),
        nr::GETPPID => process::getppid(),
        nr::FORK => process::fork(f),
        nr::EXIT => process::exit_current(Termination::exit(a0 as i32).wait_status()),
        nr::WAIT4 => process::wait4(a0 as i64, a1, a2),
        nr::PAUSE => process::pause(),
        nr::KILL => process::kill(a0 as i64, a1),
        nr::KILL_PGRP => process::kill_pgrp(a0, a1),
        nr::RT_SIGACTION => process::rt_sigaction(a0, a1, a2, a3),
        nr::RT_SIGPROCMASK => process::rt_sigprocmask(a0, a1, a2, a3),
        nr::RT_SIGRETURN => process::rt_sigreturn(f),
        nr::SETPGID => process::setpgid(a0, a1),
        nr::GETPGID => process::getpgid(a0),
        nr::SETSID => process::setsid(),
        nr::GETSID => process::getsid(a0),
        nr::PROC_LIST => process::proc_list(a0, a1),
        nr::IOPERM => process::ioperm(a0, a1, a2),
        nr::IPC_SEND => process::ipc_send(a0, a1, a2),
        nr::IPC_RECV => process::ipc_recv(a0),
        nr::IPC_CALL => process::ipc_call(a0, a1, a2, a3, Access::ReadWrite, false),
        nr::IPC_REPLY => process::ipc_reply(a0, a1),
        nr::IPC_READ => process::ipc_transfer(a0, a1, a2, a3, false),
        nr::IPC_WRITE => process::ipc_transfer(a0, a1, a2, a3, true),
        nr::IRQ_BIND => irq::bind(a0),
        nr::IRQ_ACK => irq::ack(a0),
        nr::SVC_REGISTER => process::svc_register(a0),
        nr::SVC_LOOKUP => process::svc_lookup(a0),
        nr::DEBUG_LOG => sys_debug_log(a0, a1),
        _ => -ENOSYS,
    };
    f.rax = ret as u64;
    process::deliver_signals(f, nr);
}

/// Reads from a file descriptor (the `read` system call).
///
/// A file is read by the kernel ([`fs`]). The terminal lives in the
/// user-space console server, so a read from it is forwarded there as an IPC
/// call, which lets the server only write into the buffer: the caller's other
/// memory, and what the buffer held before, stay out of its reach.
fn sys_read(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(ofd) = process::description(fd) else {
        return -EBADF;
    };
    let pml4 = process::current_pml4();
    if !vmm::user_range_ok(pml4, buf, len, true) {
        return -EFAULT;
    }
    if len == 0 {
        return 0;
    }
    match fs::read(ofd, pml4, buf, len) {
        Transfer::Done(n) => n,
        Transfer::Console => process::console_call(OP_READ, buf, len, Access::WriteOnly),
    }
}

/// Writes to a file descriptor (the `write` system call; see [`sys_read`]).
///
/// The console server may only read the buffer of a forwarded write.
fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(ofd) = process::description(fd) else {
        return -EBADF;
    };
    let pml4 = process::current_pml4();
    if !vmm::user_range_ok(pml4, buf, len, false) {
        return -EFAULT;
    }
    if len == 0 {
        return 0;
    }
    match fs::write(ofd, pml4, buf, len) {
        Transfer::Done(n) => n,
        Transfer::Console => process::console_call(OP_WRITE, buf, len, Access::ReadOnly),
    }
}

/// Opens a file (the `open` system call); `path` is NUL-terminated.
fn sys_open(path: u64, flags: u64) -> i64 {
    let mut buf = [0u8; 2 * PATH_MAX];
    match user_string(path, &mut buf) {
        Ok(len) => process::open(&buf[..len], flags),
        Err(e) => e,
    }
}

/// Reads directory entries (the `getdents64` system call).
fn sys_getdents64(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(ofd) = process::description(fd) else {
        return -EBADF;
    };
    let pml4 = process::current_pml4();
    if !vmm::user_range_ok(pml4, buf, len, true) {
        return -EFAULT;
    }
    fs::getdents(ofd, pml4, buf, len)
}

/// Copies the NUL-terminated string at user address `ptr` into `buf` and
/// returns its length (without the NUL).
///
/// A string that does not end within `buf` is `-ENAMETOOLONG`. Bytes are
/// copied one at a time, so a string may end right before an unmapped page.
fn user_string(ptr: u64, buf: &mut [u8]) -> Result<usize, i64> {
    let pml4 = process::current_pml4();
    for (i, out) in buf.iter_mut().enumerate() {
        let mut byte = [0u8];
        if !vmm::copy_from_user(pml4, ptr.wrapping_add(i as u64), &mut byte) {
            return Err(-EFAULT);
        }
        if byte[0] == 0 {
            return Ok(i);
        }
        *out = byte[0];
    }
    Err(-ENAMETOOLONG)
}

/// Writes to the kernel debug log (COM1, shared with the kernel's own messages).
///
/// It never blocks, so servers and panic handlers can always report problems.
fn sys_debug_log(buf: u64, len: u64) -> i64 {
    /// Most bytes one call writes; more is cut off (a log line, not a file).
    const DEBUG_LOG_MAX: u64 = 4096;
    let len = len.min(DEBUG_LOG_MAX);
    let pml4 = process::current_pml4();
    if !vmm::user_range_ok(pml4, buf, len, false) {
        return -EFAULT;
    }
    let mut chunk = [0u8; 256];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(chunk.len() as u64) as usize;
        if !vmm::copy_from_user(pml4, buf + done, &mut chunk[..n]) {
            return -EFAULT;
        }
        serial::write_bytes(&chunk[..n]);
        done += n as u64;
    }
    len as i64
}
