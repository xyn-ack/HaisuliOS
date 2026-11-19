// SPDX-License-Identifier: MIT

//! The HaisuliOS process server.
//!
//! Implements POSIX processes and signals in user space, on top of the kernel's tasks.
//!
//! The kernel only knows tasks: address spaces with one or more threads, identified by task ids.
//! Everything POSIX about them lives here, in the task registered as [`svc::PROC`], which the
//! kernel makes its *task manager*:
//!
//! - Process ids, which are not task ids. Each process record names the task running it; a boot
//!   module becomes a process under its task id, a forked child gets the lowest free pid.
//! - Parents and children: `fork` copies the caller's task with the kernel's `task_fork`; the
//!   child inherits the process group, session and signal dispositions.
//! - Exit statuses: a process that ends, by `exit`, a signal or a fault, becomes a *zombie*,
//!   which is only a record here holding its POSIX wait status. Its task is gone at once, so a
//!   zombie costs the kernel nothing. `wait4` reaps zombies; the end of every process is logged
//!   with its wait status. Orphans are reparented to init, and when init ends the server powers
//!   the machine off.
//! - Process groups and sessions (`setpgid`, `setsid`, ...), and which processes `kill` reaches.
//! - Signals: dispositions, blocked and pending signals, and their effects. A default action
//!   ends (`task_kill`), stops (`task_stop`) or continues (`task_cont`) the task; a caught
//!   signal becomes an upcall (`task_upcall`) that makes the task enter its handler, and ends a
//!   `pause` or `wait4` it sleeps in here with `EINTR`. The restorer of every handler tells the
//!   server when the handler returns ([`OP_SIGRETURN`]), with the signal mask to restore.
//!
//! - Programs: `execve` has the kernel replace the caller's program with one from the file system
//!   (`task_exec`). At boot, the server also starts the system servers listed in [`SERVERS`]
//!   (`task_spawn`), such as the console server: the ones that are not needed to get this far.
//!   They run privileged, with no parent, like boot modules.
//! - Keeping the system servers running (like MINIX's reincarnation server): when one dies (of a
//!   signal, a fault or a non-zero exit status), the server starts its program again, as a new
//!   process. One that exits with status 0 has finished, and stays ended. A server that keeps
//!   dying soon after it starts ([`MAX_QUICK_FAILURES`] times in a row, each within
//!   [`STABLE_NS`] of its start) is given up, rather than restarted forever.
//! - Threads: a process is its task, with all of its threads (which the kernel starts with
//!   `clone`). Any thread may make a request: the kernel tells the server the calling thread,
//!   which gets the reply, and its task, which names the process. So `getpid` is the same in
//!   every thread, `exit` and the default action of a signal end every thread (`task_kill`),
//!   `fork` copies only the calling thread (`task_fork` of that thread) and `execve` ends the
//!   others. Several threads may wait in `pause` or `wait4` at once; a caught signal goes to one
//!   of them if there is one, and ends its wait. Signal masks and dispositions are the
//!   process's (POSIX gives every thread a mask of its own; HaisuliOS does not yet).
//!
//! Requests come as IPC calls ([`haisuli_rt::abi::proc`]); `pause` and a blocking `wait4` are
//! answered later, so the server never blocks on a client. It never calls another server either:
//! it logs to the kernel debug log.

#![no_std]
#![no_main]
// The only unsafe code hands out the process table, once (see `table`).
#![deny(unsafe_code)]

use core::fmt::{self, Write};
use haisuli_rt::abi::errno::{self, E2BIG, EAGAIN, ECHILD, EINTR, EINVAL, ENOSYS, EPERM, ESRCH};
use haisuli_rt::abi::exec::{self, ARG_MAX};
use haisuli_rt::abi::fcntl::O_RDONLY;
use haisuli_rt::abi::proc::*;
use haisuli_rt::abi::signal::{
    NSIG, SA_RESETHAND, SA_RESTART, SIGCONT, SIGKILL, SIGSEGV, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK,
};
use haisuli_rt::abi::{proc_state, svc, task_state, upcall, ProcInfo, Termination, PID_KERNEL};
use haisuli_rt::abi::{KMSG_TASK_EXIT, KMSG_TASK_FAULT, PROC_INFO_LEN, PROC_NAME_LEN, WNOHANG};
use haisuli_rt::io::{BufWriter, Sink};
use haisuli_rt::sys::{self, Message};
use signals::{default_action, Action, Disposition, SigAction, SigSet, Signals};

mod signals;

haisuli_rt::entry!(main);

/// How many processes the server keeps track of, zombies included.
const MAX_PROCS: usize = 64;
/// The highest pid.
const PID_MAX: u64 = 32767;
/// The pid of init, the first boot module, which inherits orphans.
///
/// The system shuts down when init ends.
const INIT_PID: u64 = 1;
/// The most words of a request (`OP_SIGACTION`).
const REQUEST_WORDS_MAX: usize = 10;
/// The most threads of one process that can wait in `pause` or `wait4` at once.
const MAX_WAITERS: usize = 8;
/// The file listing the system servers to start at boot, one path per line.
///
/// They run privileged: they are the ones that drive hardware and provide the
/// well-known services. Programs started any other way never are.
const SERVERS: &str = "/etc/servers";
/// The most system servers the server keeps running.
const MAX_SERVERS: usize = 8;
/// The longest path of a system server's program.
const SERVER_PATH_MAX: usize = 64;
/// How long a system server must have run for its death not to count as a failure to start: the
/// quick failures in a row start counting again from there.
const STABLE_NS: u64 = 10_000_000_000;
/// How many times in a row a system server may die soon after it started (within [`STABLE_NS`])
/// and be restarted; after that it is given up.
const MAX_QUICK_FAILURES: u32 = 5;

/// Whether a process record is in use.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Life {
    Unused,
    /// The process runs (or is stopped) in a task.
    Alive,
    /// The process has ended; its parent has not collected the wait status yet.
    Zombie,
}

/// A request that the server answers later.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Waiting {
    /// `pause`: until a handler is about to run.
    Pause,
    /// `wait4`: until a child it waits for ends.
    Wait { want: i64 },
}

/// A thread that waits for the answer to a request.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Waiter {
    /// The waiting thread (the kernel's id); 0 if the entry is free.
    thread: u64,
    what: Waiting,
}

impl Waiter {
    const NONE: Waiter = Waiter {
        thread: 0,
        what: Waiting::Pause,
    };
}

/// A process.
#[derive(Clone, Copy)]
struct Proc {
    life: Life,
    pid: u64,
    /// The parent's pid; 0 for boot modules, which nobody waits for.
    ppid: u64,
    /// Process group, inherited across `fork` and changed by `setpgid`.
    pgid: u64,
    /// Session, inherited across `fork` and changed by `setsid`.
    sid: u64,
    /// The task that runs the process; 0 once it has ended.
    tid: u64,
    /// The program name, NUL-padded.
    name: [u8; PROC_NAME_LEN],
    /// The POSIX wait status of a zombie.
    wstatus: i32,
    /// Whether a signal stopped the process.
    stopped: bool,
    signals: Signals,
    /// Its threads that wait for a request to the server.
    waiters: [Waiter; MAX_WAITERS],
}

impl Proc {
    const UNUSED: Proc = Proc {
        life: Life::Unused,
        pid: 0,
        ppid: 0,
        pgid: 0,
        sid: 0,
        tid: 0,
        name: [0; PROC_NAME_LEN],
        wstatus: 0,
        stopped: false,
        signals: Signals::DEFAULT,
        waiters: [Waiter::NONE; MAX_WAITERS],
    };

    fn alive(&self) -> bool {
        self.life == Life::Alive
    }

    fn exists(&self) -> bool {
        self.life != Life::Unused
    }

    /// Makes `thread` wait for the answer to request `what`; `false` if too many threads wait.
    fn add_waiter(&mut self, thread: u64, what: Waiting) -> bool {
        match self.waiters.iter_mut().find(|w| w.thread == 0) {
            Some(w) => {
                *w = Waiter { thread, what };
                true
            }
            None => false,
        }
    }

    /// Takes the waiting request of `thread`, if it has one.
    fn take_waiter(&mut self, thread: u64) -> Option<Waiting> {
        let w = self
            .waiters
            .iter_mut()
            .find(|w| w.thread != 0 && w.thread == thread)?;
        let what = w.what;
        *w = Waiter::NONE;
        Some(what)
    }

    /// The first thread that waits for an answer, if any.
    fn first_waiter(&self) -> Option<u64> {
        self.waiters
            .iter()
            .find(|w| w.thread != 0)
            .map(|w| w.thread)
    }

    /// The program name as text.
    fn name(&self) -> &str {
        let n = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(PROC_NAME_LEN);
        core::str::from_utf8(&self.name[..n]).unwrap_or("?")
    }
}

/// The processes a signal is sent to.
enum Target {
    Pid(u64),
    Group(u64),
    /// Every process that may be signalled: all but init and the sender.
    All,
}

/// What a `wait4` finds.
enum Reap {
    /// A child that ended: its pid and wait status (its record is freed).
    Reaped(u64, i32),
    /// Children that may still end.
    NotYet,
    /// No child to wait for.
    NoChild,
}

#[expect(unsafe_code)]
mod table {
    //! The process table, in `.bss`: too big for the stack.
    use super::Server;
    use core::sync::atomic::{AtomicBool, Ordering};

    static TAKEN: AtomicBool = AtomicBool::new(false);
    static mut SERVER: Server = Server::NEW;

    /// The server state.
    ///
    /// # Panics
    ///
    /// Panics if called more than once.
    pub(crate) fn take() -> &'static mut Server {
        assert!(!TAKEN.swap(true, Ordering::Relaxed), "table taken twice");
        // SAFETY: `TAKEN` makes this the only reference to `SERVER` there
        // ever is, and the process is single-threaded.
        unsafe { &mut *core::ptr::addr_of_mut!(SERVER) }
    }
}

/// A system server, from [`SERVERS`], which the server keeps running.
#[derive(Clone, Copy)]
struct SystemServer {
    /// Its program's path (`len` bytes of it); 0 long if the entry is free.
    path: [u8; SERVER_PATH_MAX],
    len: usize,
    /// The pid of the process that runs it; 0 while none does.
    pid: u64,
    /// When that process was started (nanoseconds since boot).
    started: u64,
    /// How many times in a row it died soon after it started.
    quick_failures: u32,
}

impl SystemServer {
    const NONE: SystemServer = SystemServer {
        path: [0; SERVER_PATH_MAX],
        len: 0,
        pid: 0,
        started: 0,
        quick_failures: 0,
    };

    fn path(&self) -> &str {
        core::str::from_utf8(&self.path[..self.len]).unwrap_or("?")
    }
}

/// The state of the process server.
pub(crate) struct Server {
    procs: [Proc; MAX_PROCS],
    /// The server's own task; its process cannot be signalled.
    me: u64,
    /// The system servers that it keeps running.
    servers: [SystemServer; MAX_SERVERS],
}

/// Writes a line to the kernel debug log.
fn log(args: fmt::Arguments<'_>) {
    let mut w = BufWriter::new(Sink::DebugLog);
    let _ = w.write_fmt(args);
    let _ = w.write_str("\n");
    w.flush();
}

/// Completes the request of thread `client` with `status`.
///
/// If the client is gone (its task was killed), there is nobody to tell.
fn reply(client: u64, status: i64) {
    let _ = sys::ipc_reply(client, status);
}

/// Reads the first `N` words of `client`'s request.
fn read_words<const N: usize>(client: u64, len: u64) -> Result<[u64; N], i64> {
    let mut bytes = [0u8; REQUEST_WORDS_MAX * 8];
    if N > REQUEST_WORDS_MAX || (len as usize) < N * 8 {
        return Err(-EINVAL);
    }
    match sys::ipc_read(client, 0, &mut bytes[..N * 8]) {
        Ok(n) if n == N * 8 => {}
        Ok(_) => return Err(-EINVAL),
        Err(e) => return Err(-e.0),
    }
    let mut words = [0u64; N];
    for (w, b) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *w = u64::from_ne_bytes(*b);
    }
    Ok(words)
}

/// Stores the wait status in the `wait4` request of thread `client`; returns its result.
fn report_reaped(client: u64, pid: u64, status: i32) -> i64 {
    match write_words(client, 2, &[status as u32 as u64]) {
        Ok(()) => pid as i64,
        Err(e) => e,
    }
}

/// Writes `words` into `client`'s request, from word `index` on.
fn write_words(client: u64, index: usize, words: &[u64]) -> Result<(), i64> {
    for (i, w) in words.iter().enumerate() {
        sys::ipc_write(client, (index + i) * 8, &w.to_ne_bytes()).map_err(|e| -e.0)?;
    }
    Ok(())
}

impl Server {
    const NEW: Server = Server {
        procs: [Proc::UNUSED; MAX_PROCS],
        me: 0,
        servers: [SystemServer::NONE; MAX_SERVERS],
    };

    // ---- The table ------------------------------------------------------------

    /// The live process that task `tid` runs.
    fn by_tid(&self, tid: u64) -> Option<usize> {
        (tid != 0)
            .then(|| self.procs.iter().position(|p| p.alive() && p.tid == tid))
            .flatten()
    }

    /// The live process `pid`.
    fn live(&self, pid: u64) -> Option<usize> {
        self.procs.iter().position(|p| p.alive() && p.pid == pid)
    }

    /// The lowest pid that is not in use, also not as a process group or session id.
    fn free_pid(&self) -> Option<u64> {
        (1..=PID_MAX).find(|&pid| {
            !self
                .procs
                .iter()
                .any(|p| p.exists() && (p.pid == pid || p.pgid == pid || p.sid == pid))
        })
    }

    /// Adopts the tasks that the kernel started from boot modules.
    ///
    /// They are the only tasks not created by the server, and they are all there before it
    /// registers: the kernel creates no tasks later. Each becomes a process under its task id,
    /// with no parent, leading its own session and process group.
    fn adopt_boot_tasks(&mut self) {
        let mut slots = self.procs.iter_mut();
        for tid in 1..=MAX_PROCS as u64 {
            let Ok(info) = sys::task_info(tid) else {
                continue;
            };
            let Some(p) = slots.next() else {
                break;
            };
            *p = Proc {
                life: Life::Alive,
                pid: tid,
                pgid: tid,
                sid: tid,
                tid,
                name: info.name,
                ..Proc::UNUSED
            };
        }
    }

    // ---- Requests ---------------------------------------------------------------

    /// Handles one message: a request, or a kernel report of an ended task.
    fn serve(&mut self, m: Message) {
        match (m.sender, m.a) {
            (PID_KERNEL, KMSG_TASK_EXIT) => self.task_ended(m.b, Termination::exit(0)),
            (PID_KERNEL, KMSG_TASK_FAULT) => self.task_ended(m.b, Termination::Signalled(SIGSEGV)),
            (PID_KERNEL, _) => {}
            (client, op) => self.request(client, m.task, op, m.b),
        }
    }

    /// The kernel reports that task `tid` ended by itself: so did its process.
    fn task_ended(&mut self, tid: u64, how: Termination) {
        if let Some(i) = self.by_tid(tid) {
            self.procs[i].tid = 0; // Already gone.
            self.terminate(i, how);
        }
    }

    /// Handles request `op` from thread `client` of task `task`, whose call buffer is `len`
    /// bytes.
    fn request(&mut self, client: u64, task: u64, op: u64, len: u64) {
        let Some(i) = self.by_tid(task) else {
            reply(client, -ESRCH);
            return;
        };
        let result = match op {
            OP_GETPID => Some(self.procs[i].pid as i64),
            OP_GETPPID => Some(self.procs[i].ppid as i64),
            OP_FORK => Some(self.fork(i, client)),
            OP_EXIT => match read_words::<1>(client, len) {
                Ok([code]) => {
                    self.terminate(i, Termination::exit(code as i32));
                    None
                }
                Err(e) => Some(e),
            },
            OP_WAIT => match read_words::<2>(client, len) {
                Ok([want, options]) => self.wait(i, client, want as i64, options),
                Err(e) => Some(e),
            },
            OP_KILL => Some(match read_words::<2>(client, len) {
                Ok([pid, sig]) => {
                    let target = match pid as i64 {
                        -1 => Target::All,
                        0 => Target::Group(self.procs[i].pgid),
                        pid if pid > 0 => Target::Pid(pid as u64),
                        pid => Target::Group(pid.unsigned_abs()),
                    };
                    self.send(i, target, sig)
                }
                Err(e) => e,
            }),
            OP_KILL_PGRP => Some(match read_words::<2>(client, len) {
                Ok([0, _]) => -EINVAL,
                Ok([pgid, sig]) => self.send(i, Target::Group(pgid), sig),
                Err(e) => e,
            }),
            OP_PAUSE => {
                if self.procs[i].add_waiter(client, Waiting::Pause) {
                    self.act(i); // Ends the pause at once for a deliverable signal.
                    self.end_wait_for_upcall(i, client);
                    None
                } else {
                    Some(-EAGAIN)
                }
            }
            OP_SIGACTION => Some(self.sigaction(client, i, len)),
            OP_SIGPROCMASK => Some(self.sigprocmask(client, i, len)),
            OP_SIGRETURN => Some(match read_words::<1>(client, len) {
                Ok([mask]) => {
                    self.procs[i].signals.blocked = SigSet::from_bits(mask).blockable();
                    0
                }
                Err(e) => e,
            }),
            OP_SETPGID => Some(match read_words::<2>(client, len) {
                Ok([pid, pgid]) => self.setpgid(i, pid, pgid),
                Err(e) => e,
            }),
            OP_GETPGID => Some(match read_words::<1>(client, len) {
                Ok([pid]) => self.attribute(i, pid, |p| p.pgid),
                Err(e) => e,
            }),
            OP_SETSID => Some(self.setsid(i)),
            OP_GETSID => Some(match read_words::<1>(client, len) {
                Ok([pid]) => self.attribute(i, pid, |p| p.sid),
                Err(e) => e,
            }),
            OP_LIST => Some(self.list(client, task, len)),
            OP_EXEC => self.exec(i, client, len),
            _ => Some(-ENOSYS),
        };
        if let Some(status) = result {
            // Signals that became deliverable (a mask that changed, a handler
            // that returned, a signal the process sent itself) act now, before
            // the reply: a handler goes to the thread that gets it, which then
            // enters it on its way back (on another CPU, the reply may reach it
            // at once). So the handler has run by the time the call returns, as
            // POSIX requires of `sigprocmask` and of `kill` to oneself.
            if self.procs[i].alive() && self.procs[i].tid == task {
                self.act_returning(i, Some(client));
            }
            reply(client, status);
        }
    }

    /// `fork`: copies process `i` into a new child process, whose only thread is a copy of the
    /// calling thread `client`.
    fn fork(&mut self, i: usize, client: u64) -> i64 {
        let (Some(slot), Some(pid)) =
            (self.procs.iter().position(|p| !p.exists()), self.free_pid())
        else {
            return -EAGAIN;
        };
        let parent = self.procs[i];
        let tid = match sys::task_fork(client) {
            Ok(tid) => tid,
            Err(e) => return -e.0,
        };
        self.procs[slot] = Proc {
            life: Life::Alive,
            pid,
            ppid: parent.pid,
            pgid: parent.pgid,
            sid: parent.sid,
            tid,
            name: parent.name,
            signals: parent.signals.for_child(),
            ..Proc::UNUSED
        };
        pid as i64
    }

    /// `execve`: makes process `i` run the program that the request of its thread `client`
    /// names (its other threads end).
    ///
    /// On success the process runs the new program, so there is no reply.
    fn exec(&mut self, i: usize, client: u64, len: u64) -> Option<i64> {
        if len > ARG_MAX as u64 {
            return Some(-E2BIG);
        }
        let mut block = [0u8; ARG_MAX];
        let block = &mut block[..len as usize];
        match sys::ipc_read(client, 0, block) {
            Ok(n) if n == block.len() => {}
            Ok(_) => return Some(-EINVAL),
            Err(e) => return Some(-e.0),
        }
        // Never privileged: only the servers that the server starts are.
        if let Err(e) = sys::task_exec(client, block, 0) {
            return Some(-e.0);
        }
        let p = &mut self.procs[i];
        // POSIX: caught signals go back to their default action (the handlers
        // are gone with the old program); ignored ones stay ignored, and the
        // mask and the pending signals stay.
        for (sig, action) in p.signals.actions.iter_mut().enumerate() {
            if action.disposition(sig as u64) == Disposition::Catch {
                *action = SigAction::DEFAULT;
            }
        }
        // The other threads are gone, and so are their requests.
        p.waiters = [Waiter::NONE; MAX_WAITERS];
        if let Ok(info) = sys::task_info(p.tid) {
            p.name = info.name;
        }
        self.act(i);
        None
    }

    /// Starts the system servers listed in [`SERVERS`], one path per line.
    ///
    /// Lines that are empty or start with `#` are skipped.
    fn start_servers(&mut self) {
        let Ok(fd) = sys::open(SERVERS, O_RDONLY) else {
            log(format_args!("[proc] no {}: no servers to start", SERVERS));
            return;
        };
        let mut buf = [0u8; 1024];
        let mut len = 0;
        while len < buf.len() {
            match sys::read(fd, &mut buf[len..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => len += n,
            }
        }
        let _ = sys::close(fd);
        let Ok(text) = core::str::from_utf8(&buf[..len]) else {
            log(format_args!("[proc] {} is not valid UTF-8", SERVERS));
            return;
        };
        for path in text.lines().map(str::trim) {
            if path.is_empty() || path.starts_with('#') {
                continue;
            }
            let Some(k) = self.servers.iter().position(|s| s.len == 0) else {
                log(format_args!(
                    "[proc] cannot keep {} running: too many servers",
                    path
                ));
                self.start_server(path);
                continue;
            };
            if path.len() > SERVER_PATH_MAX {
                log(format_args!(
                    "[proc] cannot keep {} running: path too long",
                    path
                ));
                self.start_server(path);
                continue;
            }
            let s = &mut self.servers[k];
            s.path[..path.len()].copy_from_slice(path.as_bytes());
            s.len = path.len();
            self.run_server(k);
        }
    }

    /// Starts system server `k` (see [`SystemServer`]).
    fn run_server(&mut self, k: usize) {
        let mut path = [0u8; SERVER_PATH_MAX];
        let len = self.servers[k].len;
        path[..len].copy_from_slice(&self.servers[k].path[..len]);
        let pid = self.start_server(core::str::from_utf8(&path[..len]).unwrap_or("?"));
        let s = &mut self.servers[k];
        s.pid = pid.unwrap_or(0);
        s.started = sys::monotonic_ns().unwrap_or(0);
    }

    /// Process `pid`, without a parent, has ended with `how`: if it ran a system server, starts
    /// that again (see the module documentation).
    fn server_ended(&mut self, pid: u64, how: Termination) {
        let Some(k) = self.servers.iter().position(|s| s.pid != 0 && s.pid == pid) else {
            return;
        };
        let now = sys::monotonic_ns().unwrap_or(0);
        let s = &mut self.servers[k];
        s.pid = 0;
        if how == Termination::Exited(0) {
            log(format_args!("[proc] {} has finished", s.path()));
            return;
        }
        if now.saturating_sub(s.started) >= STABLE_NS {
            s.quick_failures = 0;
        }
        s.quick_failures += 1;
        if s.quick_failures > MAX_QUICK_FAILURES {
            log(format_args!(
                "[proc] {} failed {} times in a row: not restarting it",
                s.path(),
                MAX_QUICK_FAILURES + 1
            ));
            return;
        }
        log(format_args!("[proc] restarting {}", s.path()));
        self.run_server(k);
    }

    /// Starts the program at `path` as a privileged system server: a process of its own, with no
    /// parent, leading its own session and process group, like a boot module. Returns its pid.
    fn start_server(&mut self, path: &str) -> Option<u64> {
        let (Some(slot), Some(pid)) =
            (self.procs.iter().position(|p| !p.exists()), self.free_pid())
        else {
            log(format_args!(
                "[proc] cannot start {}: no free process",
                path
            ));
            return None;
        };
        let mut block = [0u8; ARG_MAX];
        let started = exec::build(path, &[path], &[], &mut block)
            .map_err(|e| -e)
            .and_then(|len| sys::task_spawn(&block[..len], exec::PRIVILEGED).map_err(|e| e.0));
        let tid = match started {
            Ok(tid) => tid,
            Err(e) => {
                log(format_args!(
                    "[proc] cannot start {}: {}",
                    path,
                    errno::describe(e)
                ));
                return None;
            }
        };
        let name = sys::task_info(tid).map_or([0; PROC_NAME_LEN], |info| info.name);
        self.procs[slot] = Proc {
            life: Life::Alive,
            pid,
            pgid: pid,
            sid: pid,
            tid,
            name,
            ..Proc::UNUSED
        };
        log(format_args!(
            "[proc] started pid {} ({}) from {}",
            pid,
            self.procs[slot].name(),
            path
        ));
        Some(pid)
    }

    /// Ends process `i`, which becomes a zombie with wait status `how`.
    ///
    /// Its task (if it still has one) is ended, its children go to init, and its parent learns
    /// about it if it is waiting.
    fn terminate(&mut self, i: usize, how: Termination) {
        let p = &mut self.procs[i];
        if p.tid != 0 {
            let _ = sys::task_kill(p.tid);
        }
        p.life = Life::Zombie;
        p.tid = 0;
        p.wstatus = how.wait_status();
        p.stopped = false;
        p.waiters = [Waiter::NONE; MAX_WAITERS];
        p.signals.pending = SigSet::EMPTY;
        let (pid, ppid) = (p.pid, p.ppid);
        log(format_args!(
            "[proc] pid {} ({}) exited (wait status {:#x})",
            pid,
            p.name(),
            p.wstatus
        ));
        if pid == INIT_PID {
            log(format_args!("[proc] init exited - shutting down"));
            let _ = sys::power_off();
        }
        // POSIX: orphans are reparented to init.
        let mut orphans = false;
        for c in self
            .procs
            .iter_mut()
            .filter(|c| c.exists() && c.ppid == pid)
        {
            c.ppid = INIT_PID;
            orphans = true;
        }
        if ppid == 0 {
            // A boot module or a system server: nobody will wait for it.
            self.procs[i] = Proc::UNUSED;
            self.server_ended(pid, how);
        } else {
            self.wake_waiter(ppid);
        }
        if orphans {
            self.wake_waiter(INIT_PID);
        }
    }

    // ---- wait4 ------------------------------------------------------------------

    /// `wait4` by thread `client`: reaps a child of process `i` that `want` selects, or waits
    /// for one.
    ///
    /// `want` selects the children the POSIX way: a pid waits for that child, -1 for any child,
    /// 0 for the caller's process group and anything below -1 for process group `-want`.
    fn wait(&mut self, i: usize, client: u64, want: i64, options: u64) -> Option<i64> {
        match self.reap(i, want) {
            Reap::Reaped(pid, status) => Some(report_reaped(client, pid, status)),
            Reap::NoChild => Some(-ECHILD),
            Reap::NotYet if options & WNOHANG != 0 => Some(0),
            Reap::NotYet if !self.procs[i].add_waiter(client, Waiting::Wait { want }) => {
                Some(-EAGAIN)
            }
            Reap::NotYet => {
                self.act(i); // A deliverable signal interrupts the wait at once.
                self.end_wait_for_upcall(i, client);
                None
            }
        }
    }

    /// Frees the first ended child of process `i` that `want` selects.
    fn reap(&mut self, i: usize, want: i64) -> Reap {
        let (me, my_pgid) = (self.procs[i].pid, self.procs[i].pgid);
        let mut have_child = false;
        for c in self.procs.iter_mut() {
            if !c.exists() || c.ppid != me {
                continue;
            }
            let wanted = match want {
                -1 => true,
                0 => c.pgid == my_pgid,
                w if w > 0 => c.pid == w as u64,
                w => c.pgid == w.unsigned_abs(),
            };
            if !wanted {
                continue;
            }
            have_child = true;
            if c.life == Life::Zombie {
                let reaped = Reap::Reaped(c.pid, c.wstatus);
                *c = Proc::UNUSED;
                return reaped;
            }
        }
        if have_child {
            Reap::NotYet
        } else {
            Reap::NoChild
        }
    }

    /// Completes the `wait4`s that threads of process `pid` are blocked in, if a child they
    /// wait for ended (or none is left).
    fn wake_waiter(&mut self, pid: u64) {
        let Some(i) = self.live(pid) else {
            return;
        };
        for w in 0..MAX_WAITERS {
            let Waiter {
                thread,
                what: Waiting::Wait { want },
            } = self.procs[i].waiters[w]
            else {
                continue;
            };
            if thread == 0 {
                continue;
            }
            let status = match self.reap(i, want) {
                Reap::Reaped(child, status) => report_reaped(thread, child, status),
                Reap::NoChild => -ECHILD,
                Reap::NotYet => continue,
            };
            self.procs[i].waiters[w] = Waiter::NONE;
            reply(thread, status);
        }
    }

    // ---- Signals ----------------------------------------------------------------
    //
    // A signal is first *posted* (`post`): it becomes pending, unless the target ignores it (and
    // does not block it).
    // Then `act` does what the pending signals that are not blocked call for:
    //
    // - A default action happens at once: the target is terminated or stopped, whoever sent the
    //   signal.
    // - A caught signal becomes an upcall: the kernel enters the handler when the target next
    //   returns to user mode. If the target sleeps in `pause` or `wait4` here, that request fails
    //   with `EINTR` (with `SA_RESTART` the kernel makes a `wait4` again after the handler); an
    //   interruptible wait in the kernel (a terminal `read`) fails with `EINTR` too.
    //
    // The kernel holds one upcall per task at a time. Further caught signals stay pending until
    // the process next talks to the server, which at the latest is when the handler returns
    // (`OP_SIGRETURN`). An upcall may also be posted while the process has a request queued here
    // that the server has not received yet; the handler then runs when that request returns, so
    // a `pause` or `wait4` that finds an upcall waiting returns at once (`end_wait_for_upcall`).
    //
    // A stopped process only reacts to SIGKILL and SIGCONT; everything else stays pending until
    // it is continued.

    /// Sends `sig` from process `from` to `target`: the common part of `kill` and `kill_pgrp`.
    ///
    /// Signal 0 only reports whether a target exists. The process server itself cannot be
    /// signalled.
    fn send(&mut self, from: usize, target: Target, sig: u64) -> i64 {
        if !signals::valid(sig) {
            return -EINVAL;
        }
        let me = self.procs[from].pid;
        let mut targets = [false; MAX_PROCS];
        let (mut found, mut refused) = (false, false);
        for (t, p) in targets.iter_mut().zip(self.procs.iter()) {
            if !p.exists() {
                continue;
            }
            let hit = match target {
                Target::All => p.pid != me && p.pid != INIT_PID,
                Target::Group(pgid) => p.pgid == pgid,
                Target::Pid(pid) => p.pid == pid,
            };
            if !hit {
                continue;
            }
            if p.tid == self.me {
                refused = true;
            } else {
                *t = true;
                found = true;
            }
        }
        if !found {
            return if refused { -EPERM } else { -ESRCH };
        }
        if sig != 0 {
            for i in (0..MAX_PROCS).filter(|&i| targets[i]) {
                self.post(i, sig, me);
            }
        }
        0
    }

    /// Posts `sig`, sent by process `sender`, to process `i`.
    fn post(&mut self, i: usize, sig: u64, sender: u64) {
        if !self.procs[i].alive() {
            return; // A zombie: nothing happens.
        }
        if sig == SIGKILL {
            self.terminate(i, Termination::Signalled(SIGKILL));
            return;
        }
        let p = &mut self.procs[i];
        let disposition = p.signals.actions[sig as usize].disposition(sig);
        if sig == SIGCONT {
            // Continuing happens whatever the disposition, even if SIGCONT is
            // ignored or blocked.
            p.signals.pending.drop_stops();
            if p.stopped {
                p.stopped = false;
                let _ = sys::task_cont(p.tid);
            }
        } else if default_action(sig) == Action::Stop {
            p.signals.pending.remove(SIGCONT);
        }
        // An ignored signal is discarded, unless it is blocked: then it stays pending until it
        // is unblocked, and what happens is up to the action at that time (POSIX leaves this
        // open; Linux does the same). So a process can block a signal while it changes its
        // action, as a shell's job does right after `fork`, without losing it meanwhile.
        if disposition != Disposition::Ignore || p.signals.blocked.contains(sig) {
            p.signals.pending.add(sig);
            p.signals.senders[sig as usize] = sender;
        }
        self.act(i);
    }

    /// Takes the actions that process `i`'s pending, unblocked signals call for, lowest first.
    ///
    /// Only one handler is entered at a time: once the kernel holds an upcall for the process,
    /// further caught signals stay pending, but default actions still happen.
    fn act(&mut self, i: usize) {
        self.act_returning(i, None);
    }

    /// [`act`](Self::act), while thread `returning` of process `i` is about to get a reply: a
    /// handler runs in it, unless a thread waits in `pause` or `wait4`.
    fn act_returning(&mut self, i: usize, returning: Option<u64>) {
        let mut upcall_held = false;
        let mut later = SigSet::EMPTY;
        loop {
            let p = &mut self.procs[i];
            if !p.alive() || p.stopped {
                return;
            }
            let deliverable = p.signals.pending.without(p.signals.blocked).without(later);
            let Some(sig) = deliverable.iter().next() else {
                return;
            };
            let act = p.signals.actions[sig as usize];
            match act.disposition(sig) {
                Disposition::Catch => {
                    if upcall_held || !self.enter_handler(i, sig, act, returning) {
                        later.add(sig);
                    }
                    upcall_held = true;
                }
                Disposition::Ignore => p.signals.pending.remove(sig),
                Disposition::Default => {
                    p.signals.pending.remove(sig);
                    match default_action(sig) {
                        Action::Term => {
                            self.terminate(i, Termination::Signalled(sig));
                            return;
                        }
                        Action::Stop => {
                            p.stopped = true;
                            let _ = sys::task_stop(p.tid);
                            return;
                        }
                        Action::Cont | Action::Ignore => {}
                    }
                }
            }
        }
    }

    /// Makes process `i` enter the handler `act` for `sig`, with an upcall.
    ///
    /// The handler runs in a thread that waits in `pause` or `wait4` here, if there is one
    /// (whose wait it ends); else in thread `returning`, which is about to get a reply; else the
    /// kernel picks a thread. Returns `false` if the kernel still holds an earlier upcall for
    /// the process.
    fn enter_handler(
        &mut self,
        i: usize,
        sig: u64,
        act: SigAction,
        returning: Option<u64>,
    ) -> bool {
        let p = &mut self.procs[i];
        let waiter = p.first_waiter();
        let thread = waiter.or(returning);
        let restart = if act.flags & SA_RESTART != 0 {
            upcall::RESTART
        } else {
            0
        };
        let request = [
            act.handler,
            act.restorer,
            sig,
            p.signals.senders[sig as usize],
            p.signals.blocked.bits(),
            restart | thread.map_or(0, |_| upcall::THREAD),
        ];
        if sys::task_upcall(thread.unwrap_or(p.tid), &request).is_err() {
            return false;
        }
        p.signals.pending.remove(sig);
        p.signals.blocked = p.signals.blocked.union(act.blocks_while_handling(sig));
        if act.flags & SA_RESETHAND != 0 {
            p.signals.actions[sig as usize] = SigAction::DEFAULT;
        }
        if let Some(thread) = waiter {
            self.interrupt_wait(i, thread);
        }
        true
    }

    /// Ends the `pause` or `wait4` that thread `thread` of process `i` sleeps in here, because
    /// a handler is about to run in it.
    ///
    /// `wait4` fails with `EINTR` (which the kernel restarts after a `SA_RESTART` handler);
    /// `pause` replies 0, which the runtime reports as `EINTR`, so that it is never restarted.
    fn interrupt_wait(&mut self, i: usize, thread: u64) {
        let status = match self.procs[i].take_waiter(thread) {
            None => return,
            Some(Waiting::Pause) => 0,
            Some(Waiting::Wait { .. }) => -EINTR,
        };
        reply(thread, status);
    }

    /// Ends the `pause` or `wait4` that thread `thread` of process `i` is about to sleep in if
    /// the kernel still holds an upcall for the process.
    ///
    /// That happens when a signal was turned into an upcall while the request was queued, not
    /// yet received: the handler runs when the request returns, so it must not sleep.
    fn end_wait_for_upcall(&mut self, i: usize, thread: u64) {
        let p = &self.procs[i];
        if p.alive() && sys::task_info(p.tid).is_ok_and(|info| info.upcall) {
            self.interrupt_wait(i, thread);
        }
    }

    /// `rt_sigaction` by thread `client`: sets (and reports) the disposition of a signal of
    /// process `i`.
    fn sigaction(&mut self, client: u64, i: usize, len: u64) -> i64 {
        let [sig, set, handler, flags, restorer, mask] = match read_words::<6>(client, len) {
            Ok(w) => w,
            Err(e) => return e,
        };
        if len < (REQUEST_WORDS_MAX * 8) as u64 || sig == 0 || !signals::valid(sig) {
            return -EINVAL;
        }
        let new = SigAction::from_words(&[handler, flags, restorer, mask]);
        if set == 1 && (!signals::can_change_disposition(sig) || !new.is_valid()) {
            return -EINVAL;
        }
        let p = &mut self.procs[i];
        if let Err(e) = write_words(client, 6, &p.signals.actions[sig as usize].to_words()) {
            return e;
        }
        if set == 1 {
            p.signals.actions[sig as usize] = new;
            if new.disposition(sig) == Disposition::Ignore {
                p.signals.pending.remove(sig); // POSIX: an ignored signal is discarded.
            }
        }
        0
    }

    /// `rt_sigprocmask` by thread `client`: changes (and reports) the blocked signals of
    /// process `i`.
    ///
    /// SIGKILL and SIGSTOP are silently left unblocked. Signals that become
    /// unblocked are delivered when the call returns.
    fn sigprocmask(&mut self, client: u64, i: usize, len: u64) -> i64 {
        let [how, set_given, set] = match read_words::<3>(client, len) {
            Ok(w) => w,
            Err(e) => return e,
        };
        if len < 4 * 8 {
            return -EINVAL;
        }
        let old = self.procs[i].signals.blocked;
        let set = SigSet::from_bits(set);
        let new = match (set_given, how) {
            (0, _) => old,
            (_, SIG_BLOCK) => old.union(set),
            (_, SIG_UNBLOCK) => old.without(set),
            (_, SIG_SETMASK) => set,
            _ => return -EINVAL,
        };
        if let Err(e) = write_words(client, 3, &[old.bits()]) {
            return e;
        }
        self.procs[i].signals.blocked = new.blockable();
        0
    }

    // ---- Process groups and sessions --------------------------------------------------

    /// `setpgid`: puts `pid` (0: the caller `i`) into process group `pgid` (0: `pid`).
    ///
    /// As POSIX requires, only the caller or one of its children can be moved, a session
    /// leader cannot change group, and the group must be in the caller's session.
    fn setpgid(&mut self, i: usize, pid: u64, pgid: u64) -> i64 {
        let (me, my_sid) = (self.procs[i].pid, self.procs[i].sid);
        let pid = if pid == 0 { me } else { pid };
        let Some(t) = self.live(pid) else {
            return -ESRCH;
        };
        let target = &self.procs[t];
        if target.pid != me && target.ppid != me {
            return -ESRCH;
        }
        if target.pid == target.sid || target.sid != my_sid {
            return -EPERM; // A session leader cannot leave its group.
        }
        let pgid = if pgid == 0 { pid } else { pgid };
        // The group must be the target's own, or an existing one in the session.
        if pgid != pid
            && !self
                .procs
                .iter()
                .any(|p| p.alive() && p.pgid == pgid && p.sid == my_sid)
        {
            return -EPERM;
        }
        self.procs[t].pgid = pgid;
        0
    }

    /// `getpgid` and `getsid`: `get` of process `pid` (0: the caller `i`).
    fn attribute(&self, i: usize, pid: u64, get: fn(&Proc) -> u64) -> i64 {
        let pid = if pid == 0 { self.procs[i].pid } else { pid };
        match self.live(pid) {
            Some(t) => get(&self.procs[t]) as i64,
            None => -ESRCH,
        }
    }

    /// `setsid`: makes the caller `i` the leader of a new session and process group.
    fn setsid(&mut self, i: usize) -> i64 {
        let me = self.procs[i].pid;
        // A process group leader cannot start a session: its group would
        // straddle two sessions.
        if self.procs.iter().any(|p| p.alive() && p.pgid == me) {
            return -EPERM;
        }
        let p = &mut self.procs[i];
        p.sid = me;
        p.pgid = me;
        me as i64
    }

    // ---- ps ----------------------------------------------------------------------

    /// Lists the processes, by pid, into the request (`len` bytes) of thread `client` of task
    /// `asking`.
    fn list(&self, client: u64, asking: u64, len: u64) -> i64 {
        let capacity = len as usize / PROC_INFO_LEN;
        let mut written = 0;
        let mut last = 0;
        // By increasing pid: the next one above `last`, each time.
        while written < capacity {
            let Some(p) = self
                .procs
                .iter()
                .filter(|p| p.exists() && p.pid > last)
                .min_by_key(|p| p.pid)
            else {
                break;
            };
            last = p.pid;
            let threads = match p.life {
                Life::Alive => sys::task_info(p.tid).map_or(0, |info| info.threads),
                _ => 0,
            };
            let info = ProcInfo {
                pid: p.pid,
                ppid: p.ppid,
                pgid: p.pgid,
                sid: p.sid,
                state: self.state_of(p, asking),
                threads,
                name: p.name,
            };
            if let Err(e) = sys::ipc_write(client, written * PROC_INFO_LEN, &info.to_bytes()) {
                return -e.0;
            }
            written += 1;
        }
        written as i64
    }

    /// What `ps` shows for `p`, listed for the process that task `asking` runs.
    fn state_of(&self, p: &Proc, asking: u64) -> u64 {
        if p.life == Life::Zombie {
            return proc_state::ZOMBIE;
        }
        if p.stopped {
            return proc_state::STOPPED;
        }
        // The process asking and the server are running (the one is just
        // waiting for the other's answer).
        if p.tid == asking || p.tid == self.me {
            return proc_state::RUNNING;
        }
        match sys::task_info(p.tid).map(|info| info.state) {
            Ok(task_state::READY) => proc_state::RUNNING,
            Ok(task_state::STOPPED) => proc_state::STOPPED,
            _ => proc_state::SLEEPING,
        }
    }
}

fn main(_: haisuli_rt::StartInfo) -> i32 {
    let server = table::take();
    server.me = sys::task_self();
    server.adopt_boot_tasks();
    if let Err(e) = sys::svc_register(svc::PROC) {
        log(format_args!("[proc] svc_register failed: errno {}", e.0));
        return 1;
    }
    // Only as the task manager may the server start tasks.
    server.start_servers();
    log(format_args!("[proc] serving POSIX processes"));
    loop {
        match sys::ipc_recv() {
            Ok(m) => server.serve(m),
            Err(e) => {
                log(format_args!("[proc] ipc_recv failed: errno {}", e.0));
                return 1;
            }
        }
    }
}

// Signal numbers index per-signal arrays.
const _: () = assert!(NSIG as usize <= 64);
