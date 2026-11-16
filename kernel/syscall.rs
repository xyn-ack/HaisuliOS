// SPDX-License-Identifier: GPL-2.0

//! System calls.
//!
//! Dispatches the `syscall` instruction to the kernel's system call implementations.
//!
//! System calls use the Linux x86_64 register convention and, for files and memory, the Linux
//! numbers, so that a future libc port is easy: `rax` holds the number, the arguments are in
//! `rdi rsi rdx r10 r8 r9` and the result is returned in `rax` (a negative errno on error). POSIX
//! process management is not here: the process server implements it on top of the task control
//! calls.

use crate::arch::{UserContext, UserRegs};
use crate::fs::{self, Transfer};
use crate::task::{self, Access, Why};
use crate::{irq, serial, vmm};
use haisuli_abi::console::{OP_READ, OP_WRITE};
use haisuli_abi::errno::{EBADF, EFAULT, ENAMETOOLONG, ENOSYS, EPERM};
use haisuli_abi::fcntl::PATH_MAX;
use haisuli_abi::nr;

/// Called by the architecture's system call entry with the saved user registers; the result
/// goes back to the thread with [`UserRegs::set_return`].
///
/// On the way back to user mode it takes the last step of [`task::return_to_user`]: the
/// thread may end, wait, or enter its pending upcall, which redirects `f` to a handler.
pub(crate) extern "C" fn syscall_dispatch(f: &mut UserContext) {
    let nr = f.syscall_nr();
    let [a0, a1, a2, a3, _, _] = f.syscall_args();
    let ret: i64 = match nr {
        nr::READ => sys_read(a0, a1, a2),
        nr::WRITE => sys_write(a0, a1, a2),
        nr::OPEN => sys_open(a0, a1),
        nr::CLOSE => task::close(a0),
        nr::DUP => task::dup(a0, 0),
        nr::DUP2 => task::dup2(a0, a1),
        nr::FCNTL => task::fcntl(a0, a1, a2),
        nr::GETDENTS64 => sys_getdents64(a0, a1, a2),
        nr::MMAP => task::mmap(a1, a2),
        nr::SCHED_YIELD => task::yield_now(),
        nr::CLONE => task::clone(f, a0, a1),
        nr::EXIT => task::thread_exit(),
        nr::GETTID => task::gettid(),
        nr::GETCPU => task::getcpu(a0, a1),
        nr::SCHED_GETAFFINITY => task::sched_getaffinity(a0, a1, a2),
        nr::RT_SIGRETURN => task::rt_sigreturn(f),
        nr::IOPERM => task::ioperm(a0, a1, a2),
        nr::IPC_SEND => task::ipc_send(a0, a1, a2),
        nr::IPC_RECV => task::ipc_recv(a0),
        nr::IPC_CALL => task::ipc_call(a0, a1, a2, a3, Access::ReadWrite, false),
        nr::IPC_REPLY => task::ipc_reply(a0, a1),
        nr::IPC_READ => task::ipc_transfer(a0, a1, a2, a3, false),
        nr::IPC_WRITE => task::ipc_transfer(a0, a1, a2, a3, true),
        nr::IRQ_BIND => irq::bind(a0),
        nr::IRQ_ACK => irq::ack(a0),
        nr::SVC_REGISTER => task::svc_register(a0),
        nr::SVC_LOOKUP => task::svc_lookup(a0),
        nr::SVC_WAIT => task::svc_wait(a0),
        nr::DEBUG_LOG => sys_debug_log(a0, a1),
        nr::SERIAL_WRITE => sys_serial_write(a0, a1),
        nr::TASK_SELF => task::current_tid() as i64,
        nr::TASK_EXIT => task::end_current(Why::Exit),
        nr::TASK_FORK => task::task_fork(a0),
        nr::TASK_KILL => task::task_kill(a0),
        nr::TASK_STOP => task::task_stop(a0),
        nr::TASK_CONT => task::task_cont(a0),
        nr::TASK_UPCALL => task::task_upcall(a0, a1),
        nr::TASK_INFO => task::task_info(a0, a1, a2),
        nr::TASK_EXEC => task::task_exec(a0, a1, a2, a3),
        nr::TASK_SPAWN => task::task_spawn(a0, a1, a2),
        nr::POWER_OFF => task::power_off(),
        _ => -ENOSYS,
    };
    f.set_return(ret);
    task::return_to_user(f, Some(nr));
}

/// Reads from a file descriptor (the `read` system call).
///
/// A file is read by the kernel ([`fs`]). The terminal lives in the
/// user-space console server, so a read from it is forwarded there as an IPC
/// call, which lets the server only write into the buffer: the caller's other
/// memory, and what the buffer held before, stay out of its reach.
fn sys_read(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(ofd) = task::description(fd) else {
        return -EBADF;
    };
    let space = task::current_space();
    if !vmm::user_range_ok(space, buf, len, true) {
        return -EFAULT;
    }
    if len == 0 {
        return 0;
    }
    match fs::read(ofd.index(), space, buf, len) {
        Transfer::Done(n) => n,
        Transfer::Console => task::console_call(OP_READ, buf, len, Access::WriteOnly),
    }
}

/// Writes to a file descriptor (the `write` system call; see [`sys_read`]).
///
/// The console server may only read the buffer of a forwarded write.
fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(ofd) = task::description(fd) else {
        return -EBADF;
    };
    let space = task::current_space();
    if !vmm::user_range_ok(space, buf, len, false) {
        return -EFAULT;
    }
    if len == 0 {
        return 0;
    }
    match fs::write(ofd.index(), space, buf, len) {
        Transfer::Done(n) => n,
        Transfer::Console => task::console_call(OP_WRITE, buf, len, Access::ReadOnly),
    }
}

/// Opens a file (the `open` system call); `path` is NUL-terminated.
fn sys_open(path: u64, flags: u64) -> i64 {
    let mut buf = [0u8; 2 * PATH_MAX];
    match user_string(path, &mut buf) {
        Ok(len) => task::open(&buf[..len], flags),
        Err(e) => e,
    }
}

/// Reads directory entries (the `getdents64` system call).
fn sys_getdents64(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(ofd) = task::description(fd) else {
        return -EBADF;
    };
    let space = task::current_space();
    if !vmm::user_range_ok(space, buf, len, true) {
        return -EFAULT;
    }
    fs::getdents(ofd.index(), space, buf, len)
}

/// Copies the NUL-terminated string at user address `ptr` into `buf` and
/// returns its length (without the NUL).
///
/// A string that does not end within `buf` is `-ENAMETOOLONG`. Bytes are
/// copied one at a time, so a string may end right before an unmapped page.
fn user_string(ptr: u64, buf: &mut [u8]) -> Result<usize, i64> {
    let space = task::current_space();
    for (i, out) in buf.iter_mut().enumerate() {
        let mut byte = [0u8];
        if !vmm::copy_from_user(space, ptr.wrapping_add(i as u64), &mut byte) {
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
    serial_out(buf, len, false)
}

/// Sends bytes out of the serial line as they are (the `serial_write` system call);
/// privileged tasks only (the console server).
fn sys_serial_write(buf: u64, len: u64) -> i64 {
    if !task::current_privileged() {
        return -EPERM;
    }
    serial_out(buf, len, true)
}

/// Writes user memory `buf[..len]` to the serial line as one message: as it is (`raw`), or as
/// log text (`\n` becomes `\r\n`).
fn serial_out(buf: u64, len: u64, raw: bool) -> i64 {
    /// Most bytes one call writes; more is cut off (a log line, not a file).
    const SERIAL_OUT_MAX: u64 = 4096;
    let len = len.min(SERIAL_OUT_MAX);
    let space = task::current_space();
    if !vmm::user_range_ok(space, buf, len, false) {
        return -EFAULT;
    }
    let mut chunk = [0u8; 256];
    let mut done = 0;
    let mut log = serial::lock(); // One message, even from several CPUs at once.
    while done < len {
        let n = (len - done).min(chunk.len() as u64) as usize;
        if !vmm::copy_from_user(space, buf + done, &mut chunk[..n]) {
            return -EFAULT;
        }
        if raw {
            log.write_raw(&chunk[..n]);
        } else {
            log.write_bytes(&chunk[..n]);
        }
        done += n as u64;
    }
    len as i64
}
