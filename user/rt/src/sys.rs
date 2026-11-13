// SPDX-License-Identifier: GPL-2.0

//! System calls.
//!
//! Safe wrappers for every HaisuliOS system call; errors are returned as `Err(Errno)`.
//!
//! Every call goes through `invoke`, which takes a `Sys` value whose memory arguments are Rust
//! borrows: a pointer passed to the kernel therefore always comes from a live borrow with its exact
//! length, and from a `&mut` whenever the kernel (or, for IPC, the server) may write through it.

use crate::abi::fcntl::{F_DUPFD, PATH_MAX};
use crate::abi::signal::{SA_RESTORER, SIG_DFL, SIG_IGN};
use crate::abi::{nr, ProcInfo, PROC_INFO_LEN};
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
    /// `path` is NUL-terminated.
    Open {
        path: &'a [u8],
        flags: u64,
    },
    Close(u64),
    Dup(u64),
    Dup2 {
        fd: u64,
        newfd: u64,
    },
    DupFrom {
        fd: u64,
        min: u64,
    },
    /// The kernel fills `buf` with directory entries.
    GetDents {
        fd: u64,
        buf: &'a mut [u8],
    },
    Exit(i32),
    Fork,
    /// The kernel writes the child's wait status into `status`.
    Wait {
        pid: i64,
        status: &'a mut i32,
        options: u64,
    },
    Kill {
        pid: i64,
        sig: u64,
    },
    KillPgrp {
        pgid: u64,
        sig: u64,
    },
    Pause,
    /// The kernel reads the new disposition from `act` and writes the old one
    /// to `old`.
    SigAction {
        sig: u64,
        act: &'a KernelSigAction,
        old: &'a mut KernelSigAction,
    },
    /// The kernel reads `set` and writes the previous mask to `old`.
    SigProcMask {
        how: u64,
        set: &'a u64,
        old: &'a mut u64,
    },
    SetPgid {
        pid: u64,
        pgid: u64,
    },
    GetPgid(u64),
    SetSid,
    GetSid(u64),
    /// The kernel fills `buf` with serialised `ProcInfo` entries.
    ProcList {
        buf: &'a mut [u8],
    },
    GetPid,
    GetPpid,
    Yield,
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
        Sys::Open { path, flags } => (nr::OPEN, [ro(path).0, flags, 0o644, 0]),
        Sys::Close(fd) => (nr::CLOSE, [fd, 0, 0, 0]),
        Sys::Dup(fd) => (nr::DUP, [fd, 0, 0, 0]),
        Sys::Dup2 { fd, newfd } => (nr::DUP2, [fd, newfd, 0, 0]),
        Sys::DupFrom { fd, min } => (nr::FCNTL, [fd, F_DUPFD, min, 0]),
        Sys::GetDents { fd, buf } => {
            let (p, l) = rw(buf);
            (nr::GETDENTS64, [fd, p, l, 0])
        }
        Sys::Exit(code) => (nr::EXIT, [code as u64, 0, 0, 0]),
        Sys::Fork => (nr::FORK, [0; 4]),
        Sys::Wait {
            pid,
            status,
            options,
        } => (
            nr::WAIT4,
            [pid as u64, status as *mut i32 as u64, options, 0],
        ),
        Sys::Kill { pid, sig } => (nr::KILL, [pid as u64, sig, 0, 0]),
        Sys::KillPgrp { pgid, sig } => (nr::KILL_PGRP, [pgid, sig, 0, 0]),
        Sys::Pause => (nr::PAUSE, [0; 4]),
        Sys::SigAction { sig, act, old } => (
            nr::RT_SIGACTION,
            [
                sig,
                act as *const KernelSigAction as u64,
                old as *mut KernelSigAction as u64,
                8,
            ],
        ),
        Sys::SigProcMask { how, set, old } => (
            nr::RT_SIGPROCMASK,
            [how, set as *const u64 as u64, old as *mut u64 as u64, 8],
        ),
        Sys::SetPgid { pid, pgid } => (nr::SETPGID, [pid, pgid, 0, 0]),
        Sys::GetPgid(pid) => (nr::GETPGID, [pid, 0, 0, 0]),
        Sys::SetSid => (nr::SETSID, [0; 4]),
        Sys::GetSid(pid) => (nr::GETSID, [pid, 0, 0, 0]),
        Sys::ProcList { buf } => {
            let (p, l) = rw(buf);
            (nr::PROC_LIST, [p, l, 0, 0])
        }
        Sys::GetPid => (nr::GETPID, [0; 4]),
        Sys::GetPpid => (nr::GETPPID, [0; 4]),
        Sys::Yield => (nr::SCHED_YIELD, [0; 4]),
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
    // `Sys`; for `Write` the kernel itself forbids the server to write). The
    // remaining calls take no pointers.
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

/// Opens the file at `path` with `O_*` flags ([`crate::abi::fcntl`]) and
/// returns its file descriptor.
pub fn open(path: &str, flags: u64) -> Result<u64> {
    // The kernel wants a NUL-terminated path.
    let mut buf = [0u8; 2 * PATH_MAX + 1];
    if path.len() >= buf.len() || path.as_bytes().contains(&0) {
        return Err(Errno(crate::abi::errno::ENAMETOOLONG));
    }
    buf[..path.len()].copy_from_slice(path.as_bytes());
    check(invoke(Sys::Open {
        path: &buf[..=path.len()],
        flags,
    }))
}

/// Closes file descriptor `fd`.
pub fn close(fd: u64) -> Result<()> {
    check(invoke(Sys::Close(fd))).map(drop)
}

/// Duplicates `fd` onto the lowest free descriptor, and returns that.
pub fn dup(fd: u64) -> Result<u64> {
    check(invoke(Sys::Dup(fd)))
}

/// Duplicates `fd` onto the lowest free descriptor that is at least `min`
/// (`fcntl(fd, F_DUPFD, min)`), and returns that.
pub fn dup_from(fd: u64, min: u64) -> Result<u64> {
    check(invoke(Sys::DupFrom { fd, min }))
}

/// Makes `newfd` a copy of `fd` (closing whatever `newfd` was first).
pub fn dup2(fd: u64, newfd: u64) -> Result<u64> {
    check(invoke(Sys::Dup2 { fd, newfd }))
}

/// Fills `buf` with directory entries of directory `fd` and returns how many
/// bytes it used (0 at the end); see [`DirEntries`] to walk them.
pub fn getdents(fd: u64, buf: &mut [u8]) -> Result<usize> {
    check(invoke(Sys::GetDents { fd, buf })).map(|n| n as usize)
}

/// The entries in a buffer that [`getdents`] filled: `(name, is_directory)`.
pub struct DirEntries<'a> {
    buf: &'a [u8],
}

impl<'a> DirEntries<'a> {
    /// Walks the entries in `buf` (the bytes `getdents` reported).
    pub fn new(buf: &'a [u8]) -> Self {
        DirEntries { buf }
    }
}

impl<'a> Iterator for DirEntries<'a> {
    type Item = (&'a [u8], bool);

    fn next(&mut self) -> Option<Self::Item> {
        use crate::abi::fcntl::{DIRENT_HEADER, DT_DIR};
        let header = self.buf.get(..DIRENT_HEADER)?;
        let reclen = u16::from_ne_bytes([header[16], header[17]]) as usize;
        let record = self.buf.get(..reclen).filter(|_| reclen > DIRENT_HEADER)?;
        self.buf = &self.buf[reclen..];
        let name = &record[DIRENT_HEADER..];
        let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
        Some((name, header[18] == DT_DIR))
    }
}

/// Terminates the calling process with exit status `code`.
pub fn exit(code: i32) -> ! {
    invoke(Sys::Exit(code));
    unreachable!("exit returned")
}

/// Creates a child process; it returns 0, the parent the child's pid.
///
/// The child is a copy of the caller: same memory, same process group and
/// session, and the same ignored signals.
pub fn fork() -> Result<u64> {
    check(invoke(Sys::Fork))
}

/// Waits for a child to exit and reaps it, returning its pid and wait status.
///
/// `pid` selects the children to wait for as POSIX does: that pid, -1 for any
/// child, 0 for the caller's process group, below -1 for a process group.
/// Fails with `ECHILD` if there is no such child (including one that has
/// already been waited for).
pub fn waitpid(pid: i64, options: u64) -> Result<(u64, WaitStatus)> {
    let mut status = 0;
    let reaped = check(invoke(Sys::Wait {
        pid,
        status: &mut status,
        options,
    }))?;
    Ok((reaped, WaitStatus(status)))
}

/// Sends signal `sig` to a process (`pid` > 0), a process group (`pid` < 0) or
/// the caller's own group (`pid` == 0).
///
/// Signal 0 only checks whether a target exists.
pub fn kill(pid: i64, sig: u64) -> Result<()> {
    check(invoke(Sys::Kill { pid, sig })).map(drop)
}

/// Sends signal `sig` to every process in group `pgid`, including group 1.
///
/// `kill(-pgid, sig)` means the same, except for group 1: `kill(-1)` is the
/// POSIX broadcast to every process.
pub fn kill_pgrp(pgid: u64, sig: u64) -> Result<()> {
    check(invoke(Sys::KillPgrp { pgid, sig })).map(drop)
}

/// Sleeps until a signal arrives.
///
/// Returns `EINTR` after a signal handler has run. A signal whose default
/// action applies terminates or stops the process instead.
pub fn pause() -> Errno {
    Errno(-invoke(Sys::Pause))
}

/// A Linux `struct sigaction`, as the kernel reads and writes it.
#[repr(C)]
#[derive(Default)]
struct KernelSigAction {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

/// What a process does with a signal.
#[derive(Clone, Copy, Debug)]
pub enum SigHandler {
    /// The signal's default action.
    Default,
    /// Discard the signal.
    Ignore,
    /// Call this function with the signal number.
    ///
    /// A handler interrupts the process at the end of a system call, so it
    /// should only touch atomics (e.g. set a flag that the program checks).
    Catch(extern "C" fn(i32)),
}

/// Sets what happens when the process receives `sig`, and returns the previous disposition.
///
/// `flags` takes `SA_RESTART`, `SA_NODEFER` and `SA_RESETHAND`
/// ([`crate::abi::signal`]); `mask` is the set of signals (bits from
/// [`crate::abi::signal::bit`]) to block while the handler runs. The runtime
/// supplies the restorer that returns from a handler.
pub fn sigaction(sig: u64, handler: SigHandler, flags: u64, mask: u64) -> Result<SigHandler> {
    let act = match handler {
        SigHandler::Default => KernelSigAction {
            handler: SIG_DFL,
            ..KernelSigAction::default()
        },
        SigHandler::Ignore => KernelSigAction {
            handler: SIG_IGN,
            ..KernelSigAction::default()
        },
        SigHandler::Catch(f) => KernelSigAction {
            handler: f as usize as u64,
            flags: flags | SA_RESTORER,
            restorer: __haisuli_sigreturn as *const () as usize as u64,
            mask,
        },
    };
    let mut old = KernelSigAction::default();
    check(invoke(Sys::SigAction {
        sig,
        act: &act,
        old: &mut old,
    }))?;
    Ok(match old.handler {
        SIG_DFL => SigHandler::Default,
        SIG_IGN => SigHandler::Ignore,
        // Only this runtime installs handlers, and only `extern "C" fn(i32)`s.
        _ => SigHandler::Default,
    })
}

/// Changes the blocked signals and returns the previous set.
///
/// `how` is `SIG_BLOCK`, `SIG_UNBLOCK` or `SIG_SETMASK`; sets use the bits of
/// [`crate::abi::signal::bit`]. SIGKILL and SIGSTOP cannot be blocked.
pub fn sigprocmask(how: u64, set: u64) -> Result<u64> {
    let mut old = 0;
    check(invoke(Sys::SigProcMask {
        how,
        set: &set,
        old: &mut old,
    }))?;
    Ok(old)
}

// Where every signal handler returns to: the kernel's signal frame has this
// address as the handler's return address, and `rt_sigreturn` restores the
// interrupted context from the rest of the frame (it never returns here).
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
    /// The restorer of every handler that [`sigaction`] installs (only its
    /// address is used; it is never called from Rust).
    fn __haisuli_sigreturn();
}

/// Puts process `pid` (0 = the caller) into process group `pgid` (0 = `pid`).
pub fn setpgid(pid: u64, pgid: u64) -> Result<()> {
    check(invoke(Sys::SetPgid { pid, pgid })).map(drop)
}

/// Returns the process group of `pid` (0 = the caller).
pub fn getpgid(pid: u64) -> Result<u64> {
    check(invoke(Sys::GetPgid(pid)))
}

/// Makes the caller the leader of a new session and process group.
pub fn setsid() -> Result<u64> {
    check(invoke(Sys::SetSid))
}

/// Returns the session of `pid` (0 = the caller).
pub fn getsid(pid: u64) -> Result<u64> {
    check(invoke(Sys::GetSid(pid)))
}

/// Lists the processes of the system into `out`, returning how many it wrote.
pub fn proc_list(out: &mut [ProcInfo]) -> Result<usize> {
    let mut buf = [0u8; PROC_INFO_LEN * 16];
    let len = (out.len() * PROC_INFO_LEN).min(buf.len());
    let n = check(invoke(Sys::ProcList {
        buf: &mut buf[..len],
    }))? as usize;
    for (i, entry) in out.iter_mut().enumerate().take(n) {
        let mut one = [0u8; PROC_INFO_LEN];
        one.copy_from_slice(&buf[i * PROC_INFO_LEN..(i + 1) * PROC_INFO_LEN]);
        *entry = ProcInfo::from_bytes(&one);
    }
    Ok(n)
}

/// The POSIX wait status of a process that has exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitStatus(pub i32);

impl WaitStatus {
    /// The signal that killed the process, if one did.
    pub fn signal(self) -> Option<u64> {
        let sig = self.0 & 0x7F;
        (sig != 0).then_some(sig as u64)
    }

    /// The status the process exited with, if it was not killed.
    pub fn exit_code(self) -> Option<i32> {
        self.signal().is_none().then_some((self.0 >> 8) & 0xFF)
    }

    /// The status a shell reports in `$?`: 128 + n for a process killed by
    /// signal n, as POSIX requires.
    pub fn shell_status(self) -> i32 {
        match self.signal() {
            Some(sig) => 128 + sig as i32,
            None => (self.0 >> 8) & 0xFF,
        }
    }
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
