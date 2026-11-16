// SPDX-License-Identifier: GPL-2.0

//! System calls.
//!
//! Safe wrappers for every HaisuliOS system call, and for the POSIX process calls that the process
//! server implements; errors are returned as `Err(Errno)`.
//!
//! Every system call goes through `invoke`, which takes a `Sys` value whose memory arguments are
//! Rust borrows: a pointer passed to the kernel therefore always comes from a live borrow with its
//! exact length, and from a `&mut` whenever the kernel (or, for IPC, the server) may write through
//! it.
//!
//! The kernel knows nothing about POSIX processes. `fork`, `exit`, `waitpid`, `getpid`, `kill`,
//! process groups, sessions and signal dispositions are requests to the process server
//! ([`crate::abi::proc`]), made with `ipc_call`, like the terminal's requests to the console
//! server.

use crate::abi::device::{DeviceInfo, DEVICE_INFO_LEN};
use crate::abi::errno::{EINTR, EINVAL};
use crate::abi::fcntl::{AT_FDCWD, F_DUPFD, F_GETFD, PATH_MAX};
use crate::abi::proc::*;
use crate::abi::signal::{SA_RESTORER, SIG_DFL, SIG_IGN};
use crate::abi::time::{CLOCK_MONOTONIC, NSEC_PER_SEC, TIMESPEC_LEN};
use crate::abi::{exec, nr, svc, upcall, ProcInfo, TaskInfo, PROC_INFO_LEN, TASK_INFO_LEN};
use crate::arch;
use core::sync::atomic::{AtomicU64, Ordering};

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
    // SAFETY: The caller guarantees the arguments are valid for call `n`.
    unsafe { arch::syscall(n, a0, a1, a2, a3, a4, a5) }
}

/// The permissions `open` asks for when it creates a file: `rw-r--r--`, the
/// usual default (HaisuliOS has no file permissions yet, so it is unused).
const NEW_FILE_MODE: u64 = 0o644;

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
    OpenAt {
        dirfd: u64,
        path: &'a [u8],
        flags: u64,
    },
    Close(u64),
    Dup(u64),
    Dup3 {
        fd: u64,
        newfd: u64,
    },
    DupFrom {
        fd: u64,
        min: u64,
    },
    /// `fcntl(fd, F_GETFD)`.
    GetFd(u64),
    /// The kernel fills `buf` with directory entries.
    GetDents {
        fd: u64,
        buf: &'a mut [u8],
    },
    Yield,
    /// Ends the calling thread.
    ThreadExit,
    GetTid,
    /// The kernel writes both.
    GetCpu {
        cpu: &'a mut u32,
        node: &'a mut u32,
    },
    /// The kernel writes the mask.
    SchedGetAffinity(&'a mut [u8; 8]),
    /// The kernel writes the time.
    ClockGettime {
        clock: u64,
        out: &'a mut [u8; TIMESPEC_LEN],
    },
    Mmap {
        len: u64,
        prot: u64,
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
    SvcWait(u64),
    DebugLog(&'a [u8]),
    SerialWrite(&'a [u8]),
    TaskSelf,
    TaskExit,
    TaskFork(u64),
    TaskKill(u64),
    TaskStop(u64),
    TaskCont(u64),
    /// The kernel only reads `request`.
    TaskUpcall {
        tid: u64,
        request: &'a [u64; upcall::WORDS],
    },
    /// The kernel fills `buf` with a serialised `TaskInfo`.
    TaskInfo {
        tid: u64,
        buf: &'a mut [u8; TASK_INFO_LEN],
    },
    /// The kernel only reads `block`.
    TaskExec {
        tid: u64,
        block: &'a [u8],
        flags: u64,
    },
    /// The kernel only reads `block`.
    TaskSpawn {
        block: &'a [u8],
        flags: u64,
    },
    PowerOff,
    /// The kernel fills `buf` with a serialised `DeviceInfo`.
    DeviceInfo {
        device: u64,
        buf: &'a mut [u8; DEVICE_INFO_LEN],
    },
    DeviceGrant(u64),
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
        Sys::OpenAt { dirfd, path, flags } => {
            (nr::OPENAT, [dirfd, ro(path).0, flags, NEW_FILE_MODE])
        }
        Sys::Close(fd) => (nr::CLOSE, [fd, 0, 0, 0]),
        Sys::Dup(fd) => (nr::DUP, [fd, 0, 0, 0]),
        Sys::Dup3 { fd, newfd } => (nr::DUP3, [fd, newfd, 0, 0]),
        Sys::DupFrom { fd, min } => (nr::FCNTL, [fd, F_DUPFD, min, 0]),
        Sys::GetFd(fd) => (nr::FCNTL, [fd, F_GETFD, 0, 0]),
        Sys::GetDents { fd, buf } => {
            let (p, l) = rw(buf);
            (nr::GETDENTS64, [fd, p, l, 0])
        }
        Sys::Yield => (nr::SCHED_YIELD, [0; 4]),
        Sys::ThreadExit => (nr::EXIT, [0; 4]),
        Sys::GetTid => (nr::GETTID, [0; 4]),
        Sys::GetCpu { cpu, node } => (
            nr::GETCPU,
            [cpu as *mut u32 as u64, node as *mut u32 as u64, 0, 0],
        ),
        Sys::SchedGetAffinity(mask) => (
            nr::SCHED_GETAFFINITY,
            [0, mask.len() as u64, mask.as_mut_ptr() as u64, 0],
        ),
        Sys::ClockGettime { clock, out } => {
            (nr::CLOCK_GETTIME, [clock, out.as_mut_ptr() as u64, 0, 0])
        }
        Sys::Mmap { len, prot } => (nr::MMAP, [0, len, prot, 0]),
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
        Sys::SvcWait(id) => (nr::SVC_WAIT, [id, 0, 0, 0]),
        Sys::DebugLog(msg) => {
            let (p, l) = ro(msg);
            (nr::DEBUG_LOG, [p, l, 0, 0])
        }
        Sys::SerialWrite(bytes) => {
            let (p, l) = ro(bytes);
            (nr::SERIAL_WRITE, [p, l, 0, 0])
        }
        Sys::TaskSelf => (nr::TASK_SELF, [0; 4]),
        Sys::TaskExit => (nr::TASK_EXIT, [0; 4]),
        Sys::TaskFork(tid) => (nr::TASK_FORK, [tid, 0, 0, 0]),
        Sys::TaskKill(tid) => (nr::TASK_KILL, [tid, 0, 0, 0]),
        Sys::TaskStop(tid) => (nr::TASK_STOP, [tid, 0, 0, 0]),
        Sys::TaskCont(tid) => (nr::TASK_CONT, [tid, 0, 0, 0]),
        Sys::TaskUpcall { tid, request } => (nr::TASK_UPCALL, [tid, request.as_ptr() as u64, 0, 0]),
        Sys::TaskInfo { tid, buf } => {
            let (p, l) = rw(buf);
            (nr::TASK_INFO, [tid, p, l, 0])
        }
        Sys::TaskExec { tid, block, flags } => {
            let (p, l) = ro(block);
            (nr::TASK_EXEC, [tid, p, l, flags])
        }
        Sys::TaskSpawn { block, flags } => {
            let (p, l) = ro(block);
            (nr::TASK_SPAWN, [p, l, flags, 0])
        }
        Sys::PowerOff => (nr::POWER_OFF, [0; 4]),
        Sys::DeviceInfo { device, buf } => {
            let (p, l) = rw(buf);
            (nr::DEVICE_INFO, [device, p, l, 0])
        }
        Sys::DeviceGrant(device) => (nr::DEVICE_GRANT, [device, 0, 0, 0]),
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
///
/// There is no current directory: a relative path starts at the root.
pub fn open(path: &str, flags: u64) -> Result<u64> {
    openat(AT_FDCWD, path, flags)
}

/// Opens the file at `path` with `O_*` flags, and returns its file descriptor; a relative
/// `path` starts at directory `dirfd` ([`AT_FDCWD`]: the root).
pub fn openat(dirfd: u64, path: &str, flags: u64) -> Result<u64> {
    // The kernel wants a NUL-terminated path.
    let mut buf = [0u8; 2 * PATH_MAX + 1];
    if path.len() >= buf.len() || path.as_bytes().contains(&0) {
        return Err(Errno(crate::abi::errno::ENAMETOOLONG));
    }
    buf[..path.len()].copy_from_slice(path.as_bytes());
    check(invoke(Sys::OpenAt {
        dirfd,
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
///
/// If they are the same, this only checks that `fd` is open, as POSIX says.
pub fn dup2(fd: u64, newfd: u64) -> Result<u64> {
    if fd == newfd {
        // `dup3` refuses the same descriptor twice.
        return check(invoke(Sys::GetFd(fd))).map(|_| fd);
    }
    check(invoke(Sys::Dup3 { fd, newfd }))
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

// ---- POSIX processes (served by the process server) ----

/// Most words in a request to the process server.
const PROC_WORDS_MAX: usize = 10;

/// The task id of the process server, once known (0 until then).
static PROC_SERVER: AtomicU64 = AtomicU64::new(0);

/// The task id of the process server, waiting for it to start if need be.
fn proc_server() -> Result<u64> {
    match PROC_SERVER.load(Ordering::Relaxed) {
        0 => {
            let tid = svc_wait(svc::PROC)?;
            PROC_SERVER.store(tid, Ordering::Relaxed);
            Ok(tid)
        }
        tid => Ok(tid),
    }
}

/// Makes request `op` to the process server; `words` are its call buffer.
///
/// The server's answers (the words marked *out* in [`crate::abi::proc`]) are
/// copied back into `words`.
fn proc_call(op: u64, words: &mut [u64]) -> Result<u64> {
    let server = proc_server()?;
    let mut bytes = [0u8; PROC_WORDS_MAX * 8];
    let len = words.len() * 8;
    for (b, w) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(words.iter()) {
        *b = w.to_ne_bytes();
    }
    let result = ipc_call(server, op, &mut bytes[..len]);
    for (w, b) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *w = u64::from_ne_bytes(*b);
    }
    result
}

/// Terminates the calling process with exit status `code`.
///
/// Without a process server to tell (it is gone, or the caller is the
/// process server itself), the task just ends.
pub fn exit(code: i32) -> ! {
    let _ = proc_call(OP_EXIT, &mut [code as i64 as u64]);
    task_exit()
}

/// Replaces the calling process's program with the one at `path`, run with
/// arguments `argv` (starting with its name) and environment `envp`.
///
/// Returns only on failure: `ENOENT` if there is no such file, `EACCES` for a
/// directory, `ENOEXEC` if it is not a program and `E2BIG` if the arguments
/// are too long. The process keeps its pid and file descriptors (see
/// [`crate::abi::proc::OP_EXEC`]).
pub fn execve(path: &str, argv: &[&str], envp: &[&str]) -> Errno {
    let mut block = [0u8; exec::ARG_MAX];
    let len = match exec::build(path, argv, envp, &mut block) {
        Ok(len) => len,
        Err(e) => return Errno(-e),
    };
    let server = match proc_server() {
        Ok(server) => server,
        Err(e) => return e,
    };
    match ipc_call(server, OP_EXEC, &mut block[..len]) {
        Err(e) => e,
        Ok(_) => Errno(EINVAL), // A success does not return.
    }
}

/// Creates a child process; it returns 0, the parent the child's pid.
///
/// The child is a copy of the caller: same memory, same process group and
/// session, and the same signal dispositions and mask.
pub fn fork() -> Result<u64> {
    proc_call(OP_FORK, &mut [])
}

/// Waits for a child to exit and reaps it, returning its pid and wait status.
///
/// `pid` selects the children to wait for as POSIX does: that pid, -1 for any
/// child, 0 for the caller's process group, below -1 for a process group.
/// Fails with `ECHILD` if there is no such child (including one that has
/// already been waited for).
pub fn waitpid(pid: i64, options: u64) -> Result<(u64, WaitStatus)> {
    let mut words = [pid as u64, options, 0];
    let reaped = proc_call(OP_WAIT, &mut words)?;
    Ok((reaped, WaitStatus(words[2] as i32)))
}

/// Sends signal `sig` to a process (`pid` > 0), a process group (`pid` < 0) or
/// the caller's own group (`pid` == 0).
///
/// Signal 0 only checks whether a target exists.
pub fn kill(pid: i64, sig: u64) -> Result<()> {
    proc_call(OP_KILL, &mut [pid as u64, sig]).map(drop)
}

/// Sends signal `sig` to every process in group `pgid`, including group 1.
///
/// `kill(-pgid, sig)` means the same, except for group 1: `kill(-1)` is the
/// POSIX broadcast to every process.
pub fn kill_pgrp(pgid: u64, sig: u64) -> Result<()> {
    proc_call(OP_KILL_PGRP, &mut [pgid, sig]).map(drop)
}

/// Sleeps until a signal arrives.
///
/// Returns `EINTR` after a signal handler has run. A signal whose default
/// action applies terminates or stops the process instead.
pub fn pause() -> Errno {
    match proc_call(OP_PAUSE, &mut []) {
        Err(e) => e,
        Ok(_) => Errno(EINTR),
    }
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
    // A Linux `struct sigaction`: handler, flags, restorer, mask.
    let act = match handler {
        SigHandler::Default => [SIG_DFL, 0, 0, 0],
        SigHandler::Ignore => [SIG_IGN, 0, 0, 0],
        SigHandler::Catch(f) => [
            f as usize as u64,
            flags | SA_RESTORER,
            arch::sigreturn_restorer(),
            mask,
        ],
    };
    let mut words = [sig, 1, act[0], act[1], act[2], act[3], 0, 0, 0, 0];
    proc_call(OP_SIGACTION, &mut words)?;
    Ok(match words[6] {
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
    let mut words = [how, 1, set, 0];
    proc_call(OP_SIGPROCMASK, &mut words)?;
    Ok(words[3])
}

/// Hands the signal mask saved in a handler's frame back to the process
/// server, which then delivers the signals that it unblocks.
///
/// Called only by the architecture's signal restorer, where every handler returns to.
pub(crate) extern "C" fn restore_signal_mask(mask: u64) {
    let _ = proc_call(OP_SIGRETURN, &mut [mask]);
}

/// Puts process `pid` (0 = the caller) into process group `pgid` (0 = `pid`).
pub fn setpgid(pid: u64, pgid: u64) -> Result<()> {
    proc_call(OP_SETPGID, &mut [pid, pgid]).map(drop)
}

/// Returns the process group of `pid` (0 = the caller).
pub fn getpgid(pid: u64) -> Result<u64> {
    proc_call(OP_GETPGID, &mut [pid])
}

/// Makes the caller the leader of a new session and process group.
pub fn setsid() -> Result<u64> {
    proc_call(OP_SETSID, &mut [])
}

/// Returns the session of `pid` (0 = the caller).
pub fn getsid(pid: u64) -> Result<u64> {
    proc_call(OP_GETSID, &mut [pid])
}

/// Most processes [`proc_list`] reports.
pub const PROC_LIST_MAX: usize = 64;

/// Lists the processes of the system into `out`, returning how many it wrote.
pub fn proc_list(out: &mut [ProcInfo]) -> Result<usize> {
    let mut buf = [0u8; PROC_INFO_LEN * PROC_LIST_MAX];
    let len = (out.len() * PROC_INFO_LEN).min(buf.len());
    let n = ipc_call(proc_server()?, OP_LIST, &mut buf[..len])? as usize;
    for (entry, bytes) in out
        .iter_mut()
        .zip(buf.as_chunks::<PROC_INFO_LEN>().0)
        .take(n)
    {
        *entry = ProcInfo::from_bytes(bytes);
    }
    Ok(n)
}

/// The POSIX wait status of a process that has exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitStatus(pub i32);

impl WaitStatus {
    /// How the process ended.
    pub fn termination(self) -> crate::abi::Termination {
        crate::abi::Termination::from_wait_status(self.0)
    }

    /// The status a shell reports in `$?`: 128 + n for a process killed by
    /// signal n, as POSIX requires.
    pub fn shell_status(self) -> i32 {
        self.termination().shell_status()
    }
}

/// Returns the calling process's id (0 if the process server cannot say).
pub fn getpid() -> u64 {
    proc_call(OP_GETPID, &mut []).unwrap_or(0)
}

/// Returns the process id of the caller's parent (0 for boot modules).
pub fn getppid() -> u64 {
    proc_call(OP_GETPPID, &mut []).unwrap_or(0)
}

// ---- Tasks, threads and CPUs ----

/// Lets the other ready threads run.
pub fn sched_yield() {
    invoke(Sys::Yield);
}

/// Returns the calling thread's id (the kernel's; see [`crate::thread`]).
pub fn gettid() -> u64 {
    invoke(Sys::GetTid) as u64
}

/// Returns the number of the CPU the calling thread runs on (from 0).
///
/// It may run on another one by the time this returns.
pub fn getcpu() -> Result<u64> {
    let (mut cpu, mut node) = (0, 0);
    check(invoke(Sys::GetCpu {
        cpu: &mut cpu,
        node: &mut node,
    }))?;
    Ok(cpu as u64)
}

/// Returns the CPUs the caller may run on, one bit each (Linux `sched_getaffinity` for the
/// caller itself): every online CPU.
pub fn sched_getaffinity() -> Result<u64> {
    let mut mask = [0u8; 8];
    check(invoke(Sys::SchedGetAffinity(&mut mask)))?;
    Ok(u64::from_ne_bytes(mask))
}

/// Returns the time since boot, in nanoseconds (`clock_gettime(CLOCK_MONOTONIC)`; it advances
/// in steps of the scheduler's tick).
pub fn monotonic_ns() -> Result<u64> {
    let mut out = [0u8; TIMESPEC_LEN];
    check(invoke(Sys::ClockGettime {
        clock: CLOCK_MONOTONIC,
        out: &mut out,
    }))?;
    let (sec, nsec) = out.split_at(8);
    let word = |b: &[u8]| u64::from_ne_bytes(b.try_into().unwrap_or([0; 8]));
    Ok(word(sec) * NSEC_PER_SEC + word(nsec))
}

/// Returns how many CPUs are online, as POSIX `sysconf(_SC_NPROCESSORS_ONLN)` does (at least 1).
pub fn nprocs() -> usize {
    sched_getaffinity().map_or(1, |mask| mask.count_ones().max(1) as usize)
}

/// Ends the calling thread only (Linux `exit`; [`exit`] ends the whole process).
///
/// If it is the last thread of the process, the process ends with status 0.
pub fn thread_exit() -> ! {
    invoke(Sys::ThreadExit);
    unreachable!("thread_exit returned")
}

/// Maps `len` bytes of fresh, zeroed memory (`prot` takes [`crate::abi::mman`] flags) and
/// returns its address; the kernel picks it.
///
/// The memory is not a Rust object: it stays mapped for the life of the process (there is no
/// `munmap` yet), and what the caller does with it is up to the caller.
pub fn mmap(len: u64, prot: u64) -> Result<u64> {
    check(invoke(Sys::Mmap { len, prot }))
}

// ---- IPC ----

/// A received message: `[sender, a, b, task]`.
///
/// For a call, `a` is the operation and `b` the length of the client's buffer.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Message {
    /// Who sent it: for a call, the calling thread (reply to it); for a notification, the sending
    /// task, or [`PID_KERNEL`](crate::abi::PID_KERNEL) for the kernel's.
    pub sender: u64,
    /// The first word (the operation, for a call).
    pub a: u64,
    /// The second word (the buffer length, for a call).
    pub b: u64,
    /// The sender's task (for a call, the calling thread's; 0 for the kernel).
    pub task: u64,
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

/// Completes the call of thread `client`, whose `ipc_call` then returns `status`.
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

/// Returns the task providing service `id`.
pub fn svc_lookup(id: u64) -> Result<u64> {
    check(invoke(Sys::SvcLookup(id)))
}

/// Returns the task providing service `id`, waiting until it registers.
pub fn svc_wait(id: u64) -> Result<u64> {
    check(invoke(Sys::SvcWait(id)))
}

/// Writes to the kernel debug log, which never blocks.
pub fn debug_log(msg: &[u8]) {
    invoke(Sys::DebugLog(msg));
}

/// Sends `bytes` as they are out of the kernel's serial line, in one piece (privileged; see
/// [`crate::abi::nr::SERIAL_WRITE`]).
pub fn serial_write(bytes: &[u8]) -> Result<()> {
    check(invoke(Sys::SerialWrite(bytes))).map(drop)
}

// ---- Task control (the process server's mechanisms) ----

/// Returns the calling task's id (the kernel's, not a POSIX pid).
pub fn task_self() -> u64 {
    invoke(Sys::TaskSelf) as u64
}

/// Ends the calling task at once; the kernel tells the process server.
pub fn task_exit() -> ! {
    invoke(Sys::TaskExit);
    unreachable!("task_exit returned")
}

/// Creates a copy of the task of thread `tid`, which is blocked in a call to the caller,
/// and returns the copy's id; the copy has one thread, a copy of `tid` (task manager only).
pub fn task_fork(tid: u64) -> Result<u64> {
    check(invoke(Sys::TaskFork(tid)))
}

/// Ends task `tid` (task manager only).
pub fn task_kill(tid: u64) -> Result<()> {
    check(invoke(Sys::TaskKill(tid))).map(drop)
}

/// Stops scheduling task `tid` (task manager only).
pub fn task_stop(tid: u64) -> Result<()> {
    check(invoke(Sys::TaskStop(tid))).map(drop)
}

/// Schedules the stopped task `tid` again (task manager only).
pub fn task_cont(tid: u64) -> Result<()> {
    check(invoke(Sys::TaskCont(tid))).map(drop)
}

/// Makes a thread of task `tid` enter a handler (task manager only; see
/// [`crate::abi::upcall`]).
pub fn task_upcall(tid: u64, request: &[u64; upcall::WORDS]) -> Result<()> {
    check(invoke(Sys::TaskUpcall { tid, request })).map(drop)
}

/// Reports what task `tid` is doing.
pub fn task_info(tid: u64) -> Result<TaskInfo> {
    let mut buf = [0u8; TASK_INFO_LEN];
    check(invoke(Sys::TaskInfo { tid, buf: &mut buf }))?;
    Ok(TaskInfo::from_bytes(&buf))
}

/// Makes the task of thread `tid`, blocked in a call to the caller, run the program that
/// argument block `block` names; its other threads end (task manager only; see
/// [`crate::abi::exec`]).
pub fn task_exec(tid: u64, block: &[u8], flags: u64) -> Result<()> {
    check(invoke(Sys::TaskExec { tid, block, flags })).map(drop)
}

/// Starts the program that argument block `block` names in a new task, and
/// returns its id (task manager only; see [`crate::abi::exec`]).
pub fn task_spawn(block: &[u8], flags: u64) -> Result<u64> {
    check(invoke(Sys::TaskSpawn { block, flags }))
}

/// Powers the machine off (privileged tasks only).
pub fn power_off() -> Result<()> {
    check(invoke(Sys::PowerOff)).map(drop)
}

// ---- Devices (for drivers: see `crate::device`) ----

/// Describes device `device` (one of the ids in [`crate::abi::device`]).
pub fn device_info(device: u64) -> Result<DeviceInfo> {
    let mut buf = [0u8; DEVICE_INFO_LEN];
    check(invoke(Sys::DeviceInfo {
        device,
        buf: &mut buf,
    }))?;
    Ok(DeviceInfo::from_bytes(&buf))
}

/// Gets the registers of device `device` granted (privileged processes only): returns the
/// first I/O port, or the address where they are mapped. [`crate::device::grant`] turns that
/// into register access.
pub fn device_grant(device: u64) -> Result<u64> {
    check(invoke(Sys::DeviceGrant(device)))
}
