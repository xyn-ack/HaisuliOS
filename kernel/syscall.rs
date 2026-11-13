// SPDX-License-Identifier: GPL-2.0

//! System calls.
//!
//! Dispatches the `syscall` instruction to the kernel's system call implementations.
//!
//! System calls use the Linux x86_64 register convention and, for the POSIX subset, the Linux
//! numbers, so that a future user-space POSIX personality server or libc port is easy: `rax` holds
//! the number, the arguments are in `rdi rsi rdx r10 r8 r9` and the result is returned in `rax` (a
//! negative errno on error). As on Linux, `pid_t` arguments are 32-bit `int`s.
//!
//! Every call ends in [`process::return_to_user`], which delivers pending signals.

use crate::arch::entry::{self, TrapFrame};
use crate::process::{self, Access};
use crate::{irq, serial, vmm};
use haisuli_abi::console::{OP_GETPGRP, OP_READ, OP_SETPGRP, OP_WRITE};
use haisuli_abi::errno::{EBADF, EFAULT, ENOSYS, ENOTTY};
use haisuli_abi::nr;
use haisuli_abi::tty::{TIOCGPGRP, TIOCSPGRP};

pub(crate) fn init() {
    entry::init_syscalls();
}

/// Kernel stack for the next `syscall` (the running process's).
pub(crate) fn set_kernel_stack(top: u64) {
    entry::set_syscall_stack(top);
}

/// Called by `syscall_entry` in `arch::entry` with the saved user registers;
/// the result goes back in rax.
pub(crate) extern "C" fn syscall_dispatch(f: &mut TrapFrame) {
    let (a0, a1, a2, a3) = (f.rdi, f.rsi, f.rdx, f.r10);
    let nr = f.rax;
    let ret: i64 = match nr {
        nr::READ => sys_read(a0, a1, a2),
        nr::WRITE => sys_write(a0, a1, a2),
        nr::MMAP => process::mmap(a1, a2),
        nr::RT_SIGACTION => process::sigaction(a0, a1, a2, a3),
        nr::RT_SIGPROCMASK => process::sigprocmask(a0, a1, a2, a3),
        nr::RT_SIGRETURN => process::sigreturn(f),
        nr::IOCTL => sys_ioctl(a0, a1, a2),
        nr::SCHED_YIELD => process::yield_now(),
        nr::PAUSE => process::pause(),
        nr::GETPID => process::getpid(),
        nr::GETPPID => process::getppid(),
        nr::FORK => process::fork(f),
        nr::EXIT => process::exit_current(((a0 & 0xFF) as i32) << 8),
        nr::WAIT4 => process::wait(a0 as i32, a1, a2),
        nr::KILL => process::kill(a0 as i32, a1),
        nr::SETPGID => process::setpgid(a0 as i32, a1 as i32),
        nr::GETPGRP => process::getpgrp(),
        nr::SETSID => process::setsid(),
        nr::GETPGID => process::getpgid(a0 as i32),
        nr::GETSID => process::getsid(a0 as i32),
        nr::PRCTL => process::prctl(a0, a1),
        nr::IOPERM => process::ioperm(a0, a1, a2),
        nr::IPC_SEND => process::ipc_send(a0, a1, a2),
        nr::IPC_RECV => process::ipc_recv(a0),
        nr::IPC_CALL => process::ipc_call(a0, a1, a2, a3, Access::ReadWrite),
        nr::IPC_REPLY => process::ipc_reply(a0, a1),
        nr::IPC_READ => process::ipc_transfer(a0, a1, a2, a3, false),
        nr::IPC_WRITE => process::ipc_transfer(a0, a1, a2, a3, true),
        nr::IRQ_BIND => irq::bind(a0),
        nr::IRQ_ACK => irq::ack(a0),
        nr::SVC_REGISTER => process::svc_register(a0),
        nr::SVC_LOOKUP => process::svc_lookup(a0),
        nr::DEBUG_LOG => sys_debug_log(a0, a1),
        nr::PROC_INFO => process::proc_info(a0, a1),
        nr::KILLPG => process::killpg(a0 as i32, a1),
        _ => -ENOSYS,
    };
    f.rax = ret as u64;
    process::return_to_user(f, nr);
}

/// Forwards `read(0)` to the console server.
///
/// The terminal lives in the user-space console server: stdin, stdout and
/// stderr (`fd` 0, 1 and 2) are forwarded to it as IPC calls. The server may
/// only write into a `read()` buffer and only read from a `write()` buffer, so
/// the caller's other memory, and the contents of a buffer it merely lends,
/// stay out of its reach. There are no other files yet.
fn sys_read(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 0 {
        return -EBADF;
    }
    if !vmm::user_range_ok(process::current_pml4(), buf, len, true) {
        return -EFAULT;
    }
    if len == 0 {
        return 0;
    }
    process::console_call(OP_READ, buf, len, Access::WriteOnly)
}

/// Forwards `write(1|2)` to the console server (see [`sys_read`]).
fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return -EBADF;
    }
    if !vmm::user_range_ok(process::current_pml4(), buf, len, false) {
        return -EFAULT;
    }
    if len == 0 {
        return 0;
    }
    process::console_call(OP_WRITE, buf, len, Access::ReadOnly)
}

/// Terminal control on `fd` 0, 1 or 2: forwards `tcgetpgrp` / `tcsetpgrp` to the console server.
///
/// The server checks the request against the caller's session (see
/// [`haisuli_abi::console::OP_SETPGRP`]); it may only write the `TIOCGPGRP` buffer and only
/// read the `TIOCSPGRP` one.
fn sys_ioctl(fd: u64, request: u64, arg: u64) -> i64 {
    if fd > 2 {
        return -EBADF;
    }
    let (op, write, access) = match request {
        TIOCGPGRP => (OP_GETPGRP, true, Access::WriteOnly),
        TIOCSPGRP => (OP_SETPGRP, false, Access::ReadOnly),
        _ => return -ENOTTY,
    };
    if !vmm::user_range_ok(process::current_pml4(), arg, 4, write) {
        return -EFAULT;
    }
    process::console_call(op, arg, 4, access)
}

/// Writes to the kernel debug log (COM1, shared with the kernel's own messages).
///
/// It never blocks, so servers and panic handlers can always report problems.
fn sys_debug_log(buf: u64, len: u64) -> i64 {
    let len = len.min(4096);
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
