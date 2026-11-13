// SPDX-License-Identifier: GPL-2.0

//! System calls.
//!
//! Safe wrappers for every HaisuliOS system call; errors are returned as `Err(Errno)`.
//!
//! Every call goes through `invoke`, which takes a `Sys` value whose memory arguments are Rust
//! borrows: a pointer passed to the kernel therefore always comes from a live borrow with its exact
//! length, and from a `&mut` whenever the kernel (or, for IPC, the server) may write through it.

use crate::abi::signal::{self as sig, SA_RESTORER};
use crate::abi::{nr, tty, ProcInfo, PR_SET_NAME};
use core::arch::{asm, global_asm};

/// An error number from [`crate::abi::errno`] (positive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i64);

/// The result of a system call.
pub type Result<T> = core::result::Result<T, Errno>;

/// Raw system call, for experiments outside the safe wrappers below.
///
/// # Safety
/// The arguments must be valid for syscall `n`: in particular, any memory
/// the call reads or writes must be valid and not aliased in a way that
/// Rust's borrowing rules forbid.
#[inline(always)]
pub unsafe fn syscall(n: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
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

/// The system calls this runtime makes.
enum Sys<'a> {
    /// The kernel lets the console server only write into `buf`.
    Read {
        fd: u64,
        buf: &'a mut [u8],
    },
    /// The kernel lets the console server only read `buf` (its `ipc_write`
    /// fails with `EACCES`), so a shared borrow suffices.
    Write {
        fd: u64,
        buf: &'a [u8],
    },
    Exit(i32),
    GetPid,
    GetPpid,
    Yield,
    Fork,
    Wait4 {
        pid: i32,
        status: &'a mut i32,
        options: u64,
    },
    Kill {
        pid: i32,
        sig: u32,
    },
    KillPg {
        pgrp: i32,
        sig: u32,
    },
    Pause,
    SetPgid {
        pid: i32,
        pgid: i32,
    },
    GetPgid(i32),
    SetSid,
    GetSid(i32),
    SigAction {
        sig: u32,
        act: &'a sig::SigAction,
    },
    SigProcMask {
        how: u64,
        set: &'a u64,
        old: &'a mut u64,
    },
    /// The console server only writes `pgrp`.
    TcGetPgrp(&'a mut i32),
    /// The console server only reads `pgrp`.
    TcSetPgrp(&'a i32),
    SetName(&'a [u8; 16]),
    ProcInfo {
        pid: u64,
        buf: &'a mut [u8; ProcInfo::SIZE],
    },
    Ioperm {
        from: u64,
        num: u64,
        on: bool,
    },
    IpcSend {
        dest: u64,
        a: u64,
        b: u64,
    },
    IpcRecv(&'a mut Message),
    /// The server may read and write `buf` until it replies.
    IpcCall {
        dest: u64,
        op: u64,
        buf: &'a mut [u8],
    },
    IpcReply {
        client: u64,
        status: i64,
    },
    IpcRead {
        client: u64,
        offset: usize,
        dst: &'a mut [u8],
    },
    IpcWrite {
        client: u64,
        offset: usize,
        src: &'a [u8],
    },
    IrqBind(u64),
    IrqAck(u64),
    SvcRegister(u64),
    SvcLookup(u64),
    DebugLog(&'a [u8]),
}

fn invoke(call: Sys<'_>) -> i64 {
    fn ro(b: &[u8]) -> (u64, u64) {
        (b.as_ptr() as u64, b.len() as u64)
    }
    fn rw(b: &mut [u8]) -> (u64, u64) {
        (b.as_mut_ptr() as u64, b.len() as u64)
    }
    let (n, args) = match call {
        Sys::Read { fd, buf } => {
            let (p, l) = rw(buf);
            (nr::READ, [fd, p, l, 0])
        }
        Sys::Write { fd, buf } => {
            let (p, l) = ro(buf);
            (nr::WRITE, [fd, p, l, 0])
        }
        Sys::Exit(code) => (nr::EXIT, [code as u64, 0, 0, 0]),
        Sys::GetPid => (nr::GETPID, [0; 4]),
        Sys::GetPpid => (nr::GETPPID, [0; 4]),
        Sys::Yield => (nr::SCHED_YIELD, [0; 4]),
        Sys::Fork => (nr::FORK, [0; 4]),
        Sys::Wait4 {
            pid,
            status,
            options,
        } => (
            nr::WAIT4,
            [pid as u64, status as *mut i32 as u64, options, 0],
        ),
        Sys::Kill { pid, sig } => (nr::KILL, [pid as u64, sig as u64, 0, 0]),
        Sys::KillPg { pgrp, sig } => (nr::KILLPG, [pgrp as u64, sig as u64, 0, 0]),
        Sys::Pause => (nr::PAUSE, [0; 4]),
        Sys::SetPgid { pid, pgid } => (nr::SETPGID, [pid as u64, pgid as u64, 0, 0]),
        Sys::GetPgid(pid) => (nr::GETPGID, [pid as u64, 0, 0, 0]),
        Sys::SetSid => (nr::SETSID, [0; 4]),
        Sys::GetSid(pid) => (nr::GETSID, [pid as u64, 0, 0, 0]),
        Sys::SigAction { sig, act } => (
            nr::RT_SIGACTION,
            [sig as u64, act as *const sig::SigAction as u64, 0, 8],
        ),
        Sys::SigProcMask { how, set, old } => (
            nr::RT_SIGPROCMASK,
            [how, set as *const u64 as u64, old as *mut u64 as u64, 8],
        ),
        Sys::TcGetPgrp(pgrp) => (nr::IOCTL, [0, tty::TIOCGPGRP, pgrp as *mut i32 as u64, 0]),
        Sys::TcSetPgrp(pgrp) => (nr::IOCTL, [0, tty::TIOCSPGRP, pgrp as *const i32 as u64, 0]),
        Sys::SetName(name) => (nr::PRCTL, [PR_SET_NAME, name.as_ptr() as u64, 0, 0]),
        Sys::ProcInfo { pid, buf } => (nr::PROC_INFO, [pid, buf.as_mut_ptr() as u64, 0, 0]),
        Sys::Ioperm { from, num, on } => (nr::IOPERM, [from, num, on as u64, 0]),
        Sys::IpcSend { dest, a, b } => (nr::IPC_SEND, [dest, a, b, 0]),
        Sys::IpcRecv(m) => (nr::IPC_RECV, [m as *mut Message as u64, 0, 0, 0]),
        Sys::IpcCall { dest, op, buf } => {
            let (p, l) = rw(buf);
            (nr::IPC_CALL, [dest, op, p, l])
        }
        Sys::IpcReply { client, status } => (nr::IPC_REPLY, [client, status as u64, 0, 0]),
        Sys::IpcRead {
            client,
            offset,
            dst,
        } => {
            let (p, l) = rw(dst);
            (nr::IPC_READ, [client, offset as u64, p, l])
        }
        Sys::IpcWrite {
            client,
            offset,
            src,
        } => {
            let (p, l) = ro(src);
            (nr::IPC_WRITE, [client, offset as u64, p, l])
        }
        Sys::IrqBind(line) => (nr::IRQ_BIND, [line, 0, 0, 0]),
        Sys::IrqAck(line) => (nr::IRQ_ACK, [line, 0, 0, 0]),
        Sys::SvcRegister(id) => (nr::SVC_REGISTER, [id, 0, 0, 0]),
        Sys::SvcLookup(id) => (nr::SVC_LOOKUP, [id, 0, 0, 0]),
        Sys::DebugLog(msg) => {
            let (p, l) = ro(msg);
            (nr::DEBUG_LOG, [p, l, 0, 0])
        }
    };
    // SAFETY: Every pointer in `args` was taken from a borrow in `call`,
    // which lives until this function returns, with its exact length; it is
    // a `&mut` borrow wherever the kernel or a server can write memory (see
    // `Sys`; for `Write` and `TcSetPgrp` the kernel itself forbids the server
    // to write). The remaining calls take no pointers. `Fork` returns twice,
    // but in two separate address spaces, each continuing with its own copy
    // of every borrow.
    unsafe { syscall(n, args[0], args[1], args[2], args[3], 0, 0) }
}

fn check(r: i64) -> Result<u64> {
    if r < 0 {
        Err(Errno(-r))
    } else {
        Ok(r as u64)
    }
}

// ---- POSIX subset ----

/// Reads from `fd` into `buf` and returns the number of bytes read (0 at end of file).
pub fn read(fd: u64, buf: &mut [u8]) -> Result<usize> {
    check(invoke(Sys::Read { fd, buf })).map(|n| n as usize)
}

/// Writes `buf` to `fd` and returns the number of bytes written.
pub fn write(fd: u64, buf: &[u8]) -> Result<usize> {
    check(invoke(Sys::Write { fd, buf })).map(|n| n as usize)
}

/// Terminates the calling process with exit status `code`.
pub fn exit(code: i32) -> ! {
    invoke(Sys::Exit(code));
    unreachable!("exit returned")
}

/// Returns the calling process's id.
pub fn getpid() -> u64 {
    invoke(Sys::GetPid) as u64
}

/// Returns the process id of the caller's parent (0 for boot modules).
pub fn getppid() -> u64 {
    invoke(Sys::GetPpid) as u64
}

/// Lets the other ready processes run.
pub fn sched_yield() {
    invoke(Sys::Yield);
}

// ---- Processes, process groups and sessions ----

/// Creates a copy of the calling process; returns the child's pid, and 0 in the child.
///
/// The child has the same memory contents, process group, session, signal actions and blocked
/// signals, but no pending signals.
pub fn fork() -> Result<u64> {
    check(invoke(Sys::Fork))
}

/// Waits for a child to change state (`wait4` / `waitpid`).
///
/// `pid` > 0 selects that child, -1 any child, 0 any child in the caller's process group and
/// < -1 any child in process group `-pid`. `options` are [`abi::wait`](crate::abi::wait) flags.
/// Returns the child's pid and wait status, or `None` with `WNOHANG` if no child is ready.
pub fn waitpid(pid: i32, options: u64) -> Result<Option<(u64, i32)>> {
    let mut status = 0;
    let pid = check(invoke(Sys::Wait4 {
        pid,
        status: &mut status,
        options,
    }))?;
    Ok((pid != 0).then_some((pid, status)))
}

/// Sends signal `sig` (0: only check) to process `pid`, or to a process group (see `nr::KILL`).
pub fn kill(pid: i32, sig: u32) -> Result<()> {
    check(invoke(Sys::Kill { pid, sig })).map(drop)
}

/// Sends signal `sig` to every process in process group `pgrp`, which may be 1 (unlike `kill`).
pub fn killpg(pgrp: u64, sig: u32) -> Result<()> {
    check(invoke(Sys::KillPg {
        pgrp: pgrp as i32,
        sig,
    }))
    .map(drop)
}

/// Waits until a signal handler has run; returns the error (always `EINTR`).
pub fn pause() -> Errno {
    Errno(-invoke(Sys::Pause))
}

/// Moves process `pid` (0: the caller) into process group `pgid` (0: `pid`).
pub fn setpgid(pid: i32, pgid: i32) -> Result<()> {
    check(invoke(Sys::SetPgid { pid, pgid })).map(drop)
}

/// Returns the process group id of `pid` (0: the caller).
pub fn getpgid(pid: i32) -> Result<u64> {
    check(invoke(Sys::GetPgid(pid)))
}

/// Makes the caller the leader of a new session and process group; returns the session id.
pub fn setsid() -> Result<u64> {
    check(invoke(Sys::SetSid))
}

/// Returns the session id of `pid` (0: the caller).
pub fn getsid(pid: i32) -> Result<u64> {
    check(invoke(Sys::GetSid(pid)))
}

/// Sets the calling process's name (as `ps` shows it); at most 15 bytes are kept.
pub fn set_name(name: &str) {
    let mut buf = [0u8; 16];
    let n = name.len().min(15);
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    invoke(Sys::SetName(&buf));
}

/// Describes the process with the lowest pid >= `pid`, for listing processes.
pub fn proc_info(pid: u64) -> Result<ProcInfo> {
    let mut buf = [0u8; ProcInfo::SIZE];
    check(invoke(Sys::ProcInfo { pid, buf: &mut buf }))?;
    Ok(ProcInfo::from_bytes(&buf))
}

// ---- Signals ----

/// What to do with a signal.
#[derive(Clone, Copy)]
pub enum Handler {
    /// The default action.
    Default,
    /// Ignore it.
    Ignore,
    /// Call this function with the signal number.
    ///
    /// Handlers run only when the process is about to return from a system call, never in the
    /// middle of other code, so they may use atomics to talk to the rest of the program.
    Catch(extern "C" fn(i32)),
}

// The restorer: where handlers return to. `rt_sigreturn` restores the state from before the
// handler, so this never returns.
global_asm!(
    ".pushsection .text.__haisuli_sigreturn, \"ax\"",
    ".global __haisuli_sigreturn",
    "__haisuli_sigreturn:",
    "    mov eax, {nr}",
    "    syscall",
    "    ud2",
    ".popsection",
    nr = const nr::RT_SIGRETURN,
);

extern "C" {
    fn __haisuli_sigreturn();
}

/// Sets the action for signal `sig`.
///
/// `flags` are `SA_*` flags from [`abi::signal`](crate::abi::signal) (the runtime adds
/// `SA_RESTORER`), and `mask` the signals to block while a handler runs.
pub fn sigaction(sig: u32, handler: Handler, flags: u64, mask: u64) -> Result<()> {
    let act = sig::SigAction {
        handler: match handler {
            Handler::Default => sig::SIG_DFL,
            Handler::Ignore => sig::SIG_IGN,
            Handler::Catch(f) => f as *const () as u64,
        },
        flags: flags | SA_RESTORER,
        restorer: __haisuli_sigreturn as *const () as u64,
        mask,
    };
    check(invoke(Sys::SigAction { sig, act: &act })).map(drop)
}

/// Changes the blocked signals (`how` is `SIG_BLOCK`, `SIG_UNBLOCK` or `SIG_SETMASK`) and
/// returns the previous set.
pub fn sigprocmask(how: u64, set: u64) -> Result<u64> {
    let mut old = 0;
    check(invoke(Sys::SigProcMask {
        how,
        set: &set,
        old: &mut old,
    }))?;
    Ok(old)
}

// ---- Terminal ----

/// Returns the terminal's foreground process group (`tcgetpgrp`; 0 if there is none).
pub fn tcgetpgrp() -> Result<u64> {
    let mut pgrp = 0;
    check(invoke(Sys::TcGetPgrp(&mut pgrp)))?;
    Ok(pgrp as u64)
}

/// Makes process group `pgrp` the terminal's foreground process group (`tcsetpgrp`).
///
/// Ctrl-C then sends `SIGINT` to that group.
pub fn tcsetpgrp(pgrp: u64) -> Result<()> {
    check(invoke(Sys::TcSetPgrp(&(pgrp as i32)))).map(drop)
}

/// Grants (or revokes) access to I/O ports `[from, from + num)`.
pub fn ioperm(from: u64, num: u64, turn_on: bool) -> Result<()> {
    check(invoke(Sys::Ioperm {
        from,
        num,
        on: turn_on,
    }))
    .map(drop)
}

// ---- IPC ----

/// A received message: `[sender, a, b]`.
///
/// For a call, `a` is the operation and `b` the length of the client's buffer.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Message {
    /// The sending process, or [`PID_KERNEL`](crate::abi::PID_KERNEL) for kernel notifications.
    pub sender: u64,
    /// The first word (the operation, for a call).
    pub a: u64,
    /// The second word (the buffer length, for a call).
    pub b: u64,
}

/// Sends the asynchronous notification `[a, b]` to `dest`.
pub fn ipc_send(dest: u64, a: u64, b: u64) -> Result<()> {
    check(invoke(Sys::IpcSend { dest, a, b })).map(drop)
}

/// Waits for the next message: a notification or a call.
pub fn ipc_recv() -> Result<Message> {
    let mut m = Message::default();
    check(invoke(Sys::IpcRecv(&mut m))).map(|_| m)
}

/// Sends a synchronous request to `dest` and returns the reply status.
///
/// The server may read and write `buf` until it replies.
pub fn ipc_call(dest: u64, op: u64, buf: &mut [u8]) -> Result<u64> {
    check(invoke(Sys::IpcCall { dest, op, buf }))
}

/// Completes `client`'s call, whose `ipc_call` then returns `status`.
///
/// Negative values are seen as errors by the client.
pub fn ipc_reply(client: u64, status: i64) -> Result<()> {
    check(invoke(Sys::IpcReply { client, status })).map(drop)
}

/// Copies from `client`'s call buffer, starting at `offset`.
pub fn ipc_read(client: u64, offset: usize, dst: &mut [u8]) -> Result<usize> {
    check(invoke(Sys::IpcRead {
        client,
        offset,
        dst,
    }))
    .map(|n| n as usize)
}

/// Copies into `client`'s call buffer, starting at `offset`.
pub fn ipc_write(client: u64, offset: usize, src: &[u8]) -> Result<usize> {
    check(invoke(Sys::IpcWrite {
        client,
        offset,
        src,
    }))
    .map(|n| n as usize)
}

// ---- Drivers and services ----

/// Routes hardware IRQ `line` to this (privileged) process.
pub fn irq_bind(line: u64) -> Result<()> {
    check(invoke(Sys::IrqBind(line))).map(drop)
}

/// Re-enables IRQ `line` after its device has been serviced.
pub fn irq_ack(line: u64) -> Result<()> {
    check(invoke(Sys::IrqAck(line))).map(drop)
}

/// Registers this (privileged) process as well-known service `id`.
pub fn svc_register(id: u64) -> Result<()> {
    check(invoke(Sys::SvcRegister(id))).map(drop)
}

/// Returns the pid of the process providing service `id`.
pub fn svc_lookup(id: u64) -> Result<u64> {
    check(invoke(Sys::SvcLookup(id)))
}

/// Writes to the kernel debug log, which never blocks.
pub fn debug_log(msg: &[u8]) {
    invoke(Sys::DebugLog(msg));
}
