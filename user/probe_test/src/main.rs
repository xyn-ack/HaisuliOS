// SPDX-License-Identifier: GPL-2.0

//! `probe_test`: a test program.
//!
//! Exercises the process and signal mechanisms that the shell cannot reach, for the tests
//! (`test/test_probe.py`).
//!
//! `probe_test CASE` runs one case. It prints a line for every check, `ok` or `FAIL`, and then
//! `CASE: passed` or `CASE: failed`; it exits with status 0 only if every check passed. The cases:
//!
//! - `restart`: a `SA_RESTART` handler makes an interrupted `waitpid` again.
//! - `eintr`: without `SA_RESTART`, the interrupted `waitpid` fails with `EINTR`.
//! - `fault`: a child that causes a page fault dies of `SIGSEGV`.
//! - `task-exit`: a child that ends its task through the kernel (`task_exit`), not the process
//!   server, is reported as exited with status 0.
//! - `two-signals`: two caught signals unblocked at once both run their handlers, although the
//!   kernel holds only one upcall per task at a time.
//! - `pause-race`: a signal that becomes an upcall while the `pause` request of its target is
//!   still queued at the process server ends that `pause` (the target would sleep forever
//!   otherwise).
//! - `blocked-ignored`: a signal that is ignored but blocked when it is sent stays pending, and
//!   the action at the time it is unblocked decides (what a shell's job relies on between `fork`
//!   and resetting the shell's actions); making it ignored discards it.
//! - `files`: `openat` relative to a directory, `dup3` and `dup2`'s special cases, and on x86_64
//!   the calls that arm64 does not have (`open`, `dup2`).
//! - `devices`: what `device_info` says about the console, and that an ordinary program is not
//!   granted devices.
//! - `exec`: what `execve` keeps and what it resets, and that a failed `execve` returns.
//! - `exec-loop N`: `N` `execve`s in a row: a leak would run out of memory.

#![no_std]
#![no_main]
// The only unsafe code makes the page fault of the `fault` case.
#![deny(unsafe_code)]

use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};
use haisuli_rt::abi::errno::{E2BIG, EACCES, EBADF, EINTR, ENODEV, ENOENT, ENOEXEC};
use haisuli_rt::abi::errno::{ENOTDIR, EPERM};
use haisuli_rt::abi::fcntl::{AT_FDCWD, O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_TRUNC};
use haisuli_rt::abi::signal::{
    bit, NSIG, SA_RESTART, SIGHUP, SIGSEGV, SIGTERM, SIGUSR1, SIGUSR2, SIG_BLOCK, SIG_SETMASK,
};
use haisuli_rt::abi::{device, proc_state, ProcInfo, Termination};
use haisuli_rt::println;
use haisuli_rt::sys::{self, Errno, SigHandler, WaitStatus};

haisuli_rt::entry!(main);

/// This program, for `execve`.
const SELF: &str = "/bin/probe_test";
/// The file that the `exec` case opens before `execve` and reads after it.
const EXEC_FILE: &str = "/probe_test.exec";
/// What the `exec` case writes into [`EXEC_FILE`].
const EXEC_FILE_TEXT: &[u8] = b"kept across execve";
/// The environment that the `exec` case passes.
const EXEC_ENV: [&str; 2] = ["HOME=/", "X=1"];
/// Exit status of a child whose own checks passed (and failed).
const CHILD_OK: i32 = 0;
const CHILD_FAILED: i32 = 1;

/// How often each signal's handler ran, by signal number.
static CAUGHT: [AtomicU32; NSIG as usize] = [const { AtomicU32::new(0) }; NSIG as usize];

/// The handler of every signal the program catches: counts it.
extern "C" fn on_signal(sig: i32) {
    if let Some(count) = CAUGHT.get(sig as usize) {
        count.fetch_add(1, Ordering::Relaxed);
    }
}

/// How often the handler of `sig` ran.
fn caught(sig: u64) -> u32 {
    CAUGHT[sig as usize].load(Ordering::Relaxed)
}

/// The checks of a case, and whether one failed.
struct Checks {
    failed: bool,
}

impl Checks {
    fn new() -> Checks {
        Checks { failed: false }
    }

    /// Reports one check.
    fn check(&mut self, ok: bool, what: fmt::Arguments<'_>) {
        println!("  {} {}", if ok { "ok  " } else { "FAIL" }, what);
        self.failed |= !ok;
    }

    /// Reports the end of case `case`; returns the exit status.
    fn finish(&self, case: &str) -> i32 {
        println!(
            "{}: {}",
            case,
            if self.failed { "failed" } else { "passed" }
        );
        if self.failed {
            1
        } else {
            0
        }
    }
}

/// Lets the other tasks run for a while (the scheduler is cooperative).
fn yield_a_while() {
    for _ in 0..5 {
        sys::sched_yield();
    }
}

/// The wait status of a process that exited with `code`.
fn exited(code: i32) -> WaitStatus {
    WaitStatus(Termination::exit(code).wait_status())
}

/// The wait status of a process that signal `sig` killed.
fn killed(sig: u64) -> WaitStatus {
    WaitStatus(Termination::Signalled(sig).wait_status())
}

fn main(info: haisuli_rt::StartInfo) -> i32 {
    let mut args = info.args().skip(1);
    let Some(case) = args.next() else {
        println!("usage: probe_test CASE (see the source)");
        return 2;
    };
    let mut c = Checks::new();
    match case {
        "restart" => interrupted_wait(&mut c, true),
        "eintr" => interrupted_wait(&mut c, false),
        "fault" => fault(&mut c),
        "task-exit" => task_exit(&mut c),
        "two-signals" => two_signals(&mut c),
        "pause-race" => pause_race(&mut c),
        "blocked-ignored" => blocked_ignored(&mut c),
        "files" => files(&mut c),
        "devices" => devices(&mut c),
        "exec" => exec(&mut c),
        "exec-stage2" => exec_stage2(&mut c, info),
        "exec-loop" => {
            let Some(n) = parse(args.next()) else {
                println!("usage: probe_test exec-loop N");
                return 2;
            };
            exec_loop(&mut c, n, parse(args.next()));
        }
        _ => {
            println!("probe_test: unknown case {}", case);
            return 2;
        }
    }
    // The second stage of `exec` finishes the case it started.
    c.finish(if case == "exec-stage2" { "exec" } else { case })
}

/// Forks; `child` runs in the child, which then exits with its result.
fn fork_child(c: &mut Checks, child: impl FnOnce() -> i32) -> Option<u64> {
    match sys::fork() {
        Ok(0) => sys::exit(child()),
        Ok(pid) => Some(pid),
        Err(e) => {
            c.check(false, format_args!("fork: errno {}", e.0));
            None
        }
    }
}

/// The file with which the parent of `interrupted_wait` says that its next sleep is the wait
/// (its pid follows, so that concurrent cases stay apart).
const WAITING_PREFIX: &str = "/probe_test.waiting.";
/// How often a waiting loop looks before it gives up.
const PATIENCE: u32 = 100_000;

/// The waiting file of the `interrupted_wait` whose parent is `pid`.
fn waiting_file(pid: u64, buf: &mut [u8; 40]) -> &str {
    let prefix = WAITING_PREFIX.as_bytes();
    buf[..prefix.len()].copy_from_slice(prefix);
    let mut num = [0u8; 20];
    let digits = decimal(pid, &mut num);
    let end = prefix.len() + digits.len();
    buf[prefix.len()..end].copy_from_slice(digits.as_bytes());
    core::str::from_utf8(&buf[..end]).unwrap_or(WAITING_PREFIX)
}

/// Waits until process `pid` sleeps in a call.
///
/// For `interrupted_wait`'s child: once the parent has announced the wait (the file exists, so
/// its `fork` and the file calls, which never sleep, are behind it), the parent sleeping means
/// it is in `waitpid`: a signal now arrives during the wait, on one CPU or several.
fn wait_until_waiting(pid: u64) {
    let mut buf = [0u8; 40];
    let file = waiting_file(pid, &mut buf);
    let mut announced = false;
    for _ in 0..PATIENCE {
        if !announced {
            announced = sys::open(file, O_RDONLY).map(sys::close).is_ok();
        } else {
            let mut procs = [ProcInfo::default(); sys::PROC_LIST_MAX];
            let n = sys::proc_list(&mut procs).unwrap_or(0);
            if procs[..n]
                .iter()
                .any(|p| p.pid == pid && p.state == proc_state::SLEEPING)
            {
                return;
            }
        }
        sys::sched_yield();
    }
}

/// `restart` and `eintr`: a child signals the parent while it waits for it.
fn interrupted_wait(c: &mut Checks, restart: bool) {
    let flags = if restart { SA_RESTART } else { 0 };
    let set = sys::sigaction(SIGUSR1, SigHandler::Catch(on_signal), flags, 0);
    c.check(
        set.is_ok(),
        format_args!("catch SIGUSR1 (SA_RESTART: {})", restart),
    );
    let parent = sys::getpid();
    let Some(child) = fork_child(c, || {
        wait_until_waiting(parent); // The signal must arrive during the wait.
        let _ = sys::kill(parent as i64, SIGUSR1);
        yield_a_while();
        5
    }) else {
        return;
    };
    let mut buf = [0u8; 40];
    let announced = sys::open(waiting_file(parent, &mut buf), O_CREAT | O_RDWR).map(sys::close);
    if announced.is_err() {
        // Only on failure: printing sleeps in the console server, and the child takes any
        // sleep after the announcement for the wait.
        c.check(false, format_args!("announce the wait: {:?}", announced));
    }
    let r = sys::waitpid(child as i64, 0);
    if restart {
        c.check(
            r == Ok((child, exited(5))),
            format_args!("the wait is made again and reaps the child: {:?}", r),
        );
    } else {
        c.check(
            r == Err(Errno(EINTR)),
            format_args!("the wait fails with EINTR: {:?}", r),
        );
        let r = sys::waitpid(child as i64, 0);
        c.check(
            r == Ok((child, exited(5))),
            format_args!("a second wait reaps the child: {:?}", r),
        );
    }
    c.check(
        caught(SIGUSR1) == 1,
        format_args!("the handler ran once: {}", caught(SIGUSR1)),
    );
}

/// `fault`: a page fault in a child.
fn fault(c: &mut Checks) {
    let Some(child) = fork_child(c, || {
        page_fault();
        CHILD_OK
    }) else {
        return;
    };
    let r = sys::waitpid(child as i64, 0);
    c.check(
        r == Ok((child, killed(SIGSEGV))),
        format_args!("the child died of SIGSEGV: {:?}", r),
    );
}

/// Writes to an unmapped address.
#[expect(unsafe_code)]
fn page_fault() {
    let address = core::ptr::without_provenance_mut::<u64>(8);
    // SAFETY: Not safe at all, on purpose: the first page is never mapped, so
    // the CPU faults before anything is written, and the task ends there.
    unsafe { core::ptr::write_volatile(address, 1) };
}

/// `blocked-ignored`: what happens to a signal sent while it is blocked, depending on whether it
/// is ignored when it is sent, and when it is unblocked. Each child signals itself.
fn blocked_ignored(c: &mut Checks) {
    let cases: [(&str, [SigHandler; 3], WaitStatus); 3] = [
        (
            "ignored when sent, default when unblocked: it terminates",
            [SigHandler::Ignore, SigHandler::Default, SigHandler::Default],
            killed(SIGTERM),
        ),
        (
            "ignored when sent and when unblocked: it is discarded",
            [SigHandler::Ignore, SigHandler::Ignore, SigHandler::Ignore],
            exited(CHILD_OK),
        ),
        (
            "made ignored while pending, then default again: it was discarded",
            [SigHandler::Default, SigHandler::Ignore, SigHandler::Default],
            exited(CHILD_OK),
        ),
    ];
    for (what, [when_sent, meanwhile, when_unblocked], expected) in cases {
        // The child sets `when_sent`, blocks SIGTERM and sends it to itself, then sets
        // `meanwhile` and `when_unblocked`, and unblocks it.
        let Some(child) = fork_child(c, || {
            let ok = sys::sigaction(SIGTERM, when_sent, 0, 0).is_ok()
                && sys::sigprocmask(SIG_BLOCK, bit(SIGTERM)).is_ok()
                && sys::kill(sys::getpid() as i64, SIGTERM).is_ok()
                && sys::sigaction(SIGTERM, meanwhile, 0, 0).is_ok()
                && sys::sigaction(SIGTERM, when_unblocked, 0, 0).is_ok()
                && sys::sigprocmask(SIG_SETMASK, 0).is_ok();
            if ok {
                CHILD_OK
            } else {
                CHILD_FAILED
            }
        }) else {
            return;
        };
        let r = sys::waitpid(child as i64, 0);
        c.check(
            r == Ok((child, expected)),
            format_args!("{}: {:?}", what, r),
        );
    }
}

/// The first bytes of the file open at `fd` (at most 32), as a buffer and its length.
fn head(fd: Result<u64, Errno>) -> Option<([u8; 32], usize)> {
    let fd = fd.ok()?;
    let mut buf = [0u8; 32];
    let n = sys::read(fd, &mut buf);
    let _ = sys::close(fd);
    Some((buf, n.ok()?))
}

/// `files`: the file calls that differ between the architectures.
fn files(c: &mut Checks) {
    let motd = head(sys::open("/etc/motd", O_RDONLY));
    c.check(motd.is_some(), format_args!("open /etc/motd"));
    let relative = head(sys::openat(AT_FDCWD, "etc/motd", O_RDONLY));
    c.check(
        relative == motd,
        format_args!("openat(AT_FDCWD) starts a relative path at the root"),
    );
    let etc = sys::open("/etc", O_RDONLY | O_DIRECTORY);
    c.check(etc.is_ok(), format_args!("open /etc: {:?}", etc));
    let etc = etc.unwrap_or(u64::MAX);
    let in_etc = head(sys::openat(etc, "motd", O_RDONLY));
    c.check(
        in_etc == motd,
        format_args!("openat(/etc, \"motd\") opens /etc/motd"),
    );
    let up = head(sys::openat(etc, "../etc/./motd", O_RDONLY));
    c.check(up == motd, format_args!("openat(/etc, \"../etc/./motd\")"));
    let absolute = head(sys::openat(etc, "/etc/motd", O_RDONLY));
    c.check(
        absolute == motd,
        format_args!("an absolute path ignores the directory"),
    );
    let file = sys::open("/etc/motd", O_RDONLY).unwrap_or(u64::MAX);
    for (dirfd, what, expected) in [
        (file, "a file", ENOTDIR),
        (0, "the terminal", ENOTDIR),
        (13, "a descriptor that is not open", EBADF),
    ] {
        let r = sys::openat(dirfd, "motd", O_RDONLY);
        c.check(
            r == Err(Errno(expected)),
            format_args!("openat relative to {}: {:?}", what, r),
        );
    }
    let _ = sys::close(etc);

    // `dup2`'s special case, which `dup3` does not have.
    let same = sys::dup2(file, file);
    c.check(
        same == Ok(file),
        format_args!("dup2(fd, fd) returns fd: {:?}", same),
    );
    let closed = sys::dup2(13, 13);
    c.check(
        closed == Err(Errno(EBADF)),
        format_args!("dup2(fd, fd) of a closed fd: {:?}", closed),
    );
    let copy = sys::dup2(file, 12);
    c.check(copy == Ok(12), format_args!("dup2 onto 12: {:?}", copy));
    let _ = sys::close(12);
    let _ = sys::close(file);
    legacy_calls(c);
}

/// Calls a system call by number (for the calls that the runtime does not make).
#[cfg(target_arch = "x86_64")]
#[expect(unsafe_code)]
fn raw_call(n: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    // SAFETY: Only used below, with pointers to live, NUL-terminated buffers where the call reads
    // a path, and nothing the kernel writes.
    unsafe { sys::syscall(n, a0, a1, a2, 0, 0, 0) }
}

/// On x86_64: `open`, `dup2` and `dup3`'s checks by number; `ioperm` is gone (devices are granted
/// instead).
#[cfg(target_arch = "x86_64")]
fn legacy_calls(c: &mut Checks) {
    use haisuli_rt::abi::errno::{EINVAL, ENOSYS};
    use haisuli_rt::abi::nr;
    let path = b"/etc/motd\0";
    let fd = raw_call(nr::OPEN, path.as_ptr() as u64, O_RDONLY, 0);
    c.check(fd >= 0, format_args!("x86_64's open: {}", fd));
    let copy = raw_call(nr::DUP2, fd as u64, 11, 0);
    c.check(copy == 11, format_args!("x86_64's dup2: {}", copy));
    let same = raw_call(nr::DUP3, fd as u64, fd as u64, 0);
    c.check(
        same == -EINVAL,
        format_args!("dup3(fd, fd) fails with EINVAL: {}", same),
    );
    let flags = raw_call(nr::DUP3, fd as u64, 10, 1);
    c.check(
        flags == -EINVAL,
        format_args!("dup3 with flags fails with EINVAL: {}", flags),
    );
    let ioperm = raw_call(173, 0x3F8, 8, 1);
    c.check(
        ioperm == -ENOSYS,
        format_args!("there is no ioperm: {}", ioperm),
    );
    for fd in [fd as u64, 11] {
        let _ = sys::close(fd);
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn legacy_calls(_: &mut Checks) {}

/// `devices`: the console's description, and what an unprivileged program may not do.
fn devices(c: &mut Checks) {
    use device::{KIND_UART_16550, KIND_UART_PL011, NO_IRQ, SPACE_MMIO, SPACE_PORTS};
    match sys::device_info(device::CONSOLE) {
        Ok(info) => {
            println!(
                "console: {} kind {} space {} base {:#x} len {} irq {}",
                info.name(),
                info.kind,
                info.space,
                info.base,
                info.len,
                info.irq
            );
            c.check(
                [KIND_UART_16550, KIND_UART_PL011].contains(&info.kind)
                    && [SPACE_PORTS, SPACE_MMIO].contains(&info.space)
                    && info.len > 0
                    && info.irq != NO_IRQ
                    && !info.name().is_empty(),
                format_args!("the console is a UART with registers and an interrupt"),
            );
        }
        Err(e) => c.check(false, format_args!("device_info(CONSOLE): {:?}", e)),
    }
    let none = sys::device_info(99);
    c.check(
        none == Err(Errno(ENODEV)),
        format_args!("no device 99: {:?}", none),
    );
    for id in [device::CONSOLE, 99] {
        let grant = sys::device_grant(id);
        c.check(
            grant == Err(Errno(EPERM)),
            format_args!(
                "an ordinary program is granted no device ({}): {:?}",
                id, grant
            ),
        );
    }
}

/// `task-exit`: a child ends its task behind the process server's back.
fn task_exit(c: &mut Checks) {
    let Some(child) = fork_child(c, || sys::task_exit()) else {
        return;
    };
    let r = sys::waitpid(child as i64, 0);
    c.check(
        r == Ok((child, exited(0))),
        format_args!("the child is reported as exited with status 0: {:?}", r),
    );
}

/// `two-signals`: two blocked signals, unblocked at once.
fn two_signals(c: &mut Checks) {
    let both = bit(SIGUSR1) | bit(SIGUSR2);
    for sig in [SIGUSR1, SIGUSR2] {
        let set = sys::sigaction(sig, SigHandler::Catch(on_signal), 0, 0);
        c.check(set.is_ok(), format_args!("catch signal {}", sig));
    }
    let blocked = sys::sigprocmask(SIG_BLOCK, both);
    c.check(blocked.is_ok(), format_args!("block them"));
    let me = sys::getpid() as i64;
    let sent = (sys::kill(me, SIGUSR1), sys::kill(me, SIGUSR2));
    c.check(
        sent == (Ok(()), Ok(())),
        format_args!("send both: {:?}", sent),
    );
    c.check(
        (caught(SIGUSR1), caught(SIGUSR2)) == (0, 0),
        format_args!("while blocked, no handler runs"),
    );
    let old = sys::sigprocmask(SIG_SETMASK, 0);
    c.check(
        old == Ok(both),
        format_args!("unblock both (the mask was {:?})", old),
    );
    let counts = (caught(SIGUSR1), caught(SIGUSR2));
    c.check(
        counts == (1, 1),
        format_args!(
            "both handlers ran once by the time sigprocmask returned: {:?}",
            counts
        ),
    );
    let mask = sys::sigprocmask(SIG_BLOCK, 0);
    c.check(
        mask == Ok(0),
        format_args!("the handlers restored the mask: {:?}", mask),
    );
}

/// `pause-race`: the parent signals a child that is about to pause.
///
/// After `fork`, the cooperative scheduler runs the parent first: its `kill` reaches the process
/// server's queue. Then the child runs, and its `pause` queues up behind the `kill` before the
/// server gets to either. So the server turns the signal into an upcall while the `pause`
/// request is still waiting to be received.
fn pause_race(c: &mut Checks) {
    let set = sys::sigaction(SIGUSR1, SigHandler::Catch(on_signal), 0, 0);
    c.check(set.is_ok(), format_args!("catch SIGUSR1"));
    let Some(child) = fork_child(c, || {
        // The child's first request: nothing may come before it.
        let e = sys::pause();
        if e == Errno(EINTR) && caught(SIGUSR1) == 1 {
            CHILD_OK
        } else {
            CHILD_FAILED
        }
    }) else {
        return;
    };
    // Nothing may come before this either.
    let sent = sys::kill(child as i64, SIGUSR1);
    c.check(sent.is_ok(), format_args!("signal the child: {:?}", sent));
    let r = sys::waitpid(child as i64, 0);
    c.check(
        r == Ok((child, exited(CHILD_OK))),
        format_args!(
            "the child's pause ended with EINTR once its handler ran: {:?}",
            r
        ),
    );
}

/// `exec`, first stage: failed `execve`s, then one that keeps the program's state.
fn exec(c: &mut Checks) {
    for (path, errno, what) in [
        ("/bin/nope", ENOENT, "a missing file"),
        ("/etc/motd", ENOEXEC, "a file that is not a program"),
        ("/bin", EACCES, "a directory"),
    ] {
        let e = sys::execve(path, &[path], &[]);
        c.check(
            e == Errno(errno),
            format_args!("execve of {} fails: errno {}", what, e.0),
        );
    }
    let big = [b'x'; 3000];
    let big = core::str::from_utf8(&big).unwrap_or("");
    let e = sys::execve(SELF, &["probe_test", big, big], &[]);
    c.check(
        e == Errno(E2BIG),
        format_args!("execve of too many arguments fails: errno {}", e.0),
    );
    // What must cross the `execve`: the pids, a descriptor, the mask, the
    // ignored signals; and what must not: the handlers.
    let prepared = sys::sigaction(SIGUSR1, SigHandler::Catch(on_signal), 0, 0).is_ok()
        && sys::sigaction(SIGUSR2, SigHandler::Ignore, 0, 0).is_ok()
        && sys::sigprocmask(SIG_SETMASK, bit(SIGHUP)).is_ok();
    c.check(
        prepared,
        format_args!("catch SIGUSR1, ignore SIGUSR2, block SIGHUP"),
    );
    let fd = match sys::open(EXEC_FILE, O_CREAT | O_RDWR | O_TRUNC) {
        Ok(fd) => {
            let _ = sys::write(fd, EXEC_FILE_TEXT);
            let _ = sys::close(fd);
            sys::open(EXEC_FILE, O_RDONLY)
        }
        Err(e) => Err(e),
    };
    c.check(fd.is_ok(), format_args!("open {}: {:?}", EXEC_FILE, fd));
    let Ok(fd) = fd else {
        return;
    };
    if c.failed {
        return;
    }
    let (mut pid, mut ppid, mut fd_buf) = ([0u8; 20], [0u8; 20], [0u8; 20]);
    let argv = [
        "probe_test",
        "exec-stage2",
        decimal(sys::getpid(), &mut pid),
        decimal(sys::getppid(), &mut ppid),
        decimal(fd, &mut fd_buf),
        "two words",
        "ünï",
    ];
    let e = sys::execve(SELF, &argv, &EXEC_ENV);
    c.check(
        false,
        format_args!("execve of probe_test itself: errno {}", e.0),
    );
}

/// `n` in decimal, written into `buf`.
fn decimal(mut n: u64, buf: &mut [u8; 20]) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("0")
}

/// Parses what [`decimal`] wrote.
fn parse(s: Option<&str>) -> Option<u64> {
    s?.parse().ok()
}

/// `exec`, second stage: the program after `execve`.
///
/// Its arguments are `exec-stage2 PID PPID FD` and then two more to check.
fn exec_stage2(c: &mut Checks, info: haisuli_rt::StartInfo) {
    let mut args = info.args();
    let expected = ["probe_test", "exec-stage2"];
    let head_ok = expected.iter().all(|&e| args.next() == Some(e));
    let (pid, ppid, fd) = (parse(args.next()), parse(args.next()), parse(args.next()));
    let tail_ok = args.next() == Some("two words") && args.next() == Some("ünï");
    let all_read = args.next().is_none();
    c.check(
        head_ok && tail_ok && all_read && pid.is_some() && ppid.is_some() && fd.is_some(),
        format_args!("the arguments arrived intact"),
    );
    let mut env = info.env();
    let env_ok = EXEC_ENV.iter().all(|&e| env.next() == Some(e)) && env.next().is_none();
    c.check(env_ok, format_args!("the environment arrived intact"));
    let (now_pid, now_ppid) = (sys::getpid(), sys::getppid());
    c.check(
        Some(now_pid) == pid && Some(now_ppid) == ppid,
        format_args!("same pid and parent: {} {}", now_pid, now_ppid),
    );
    let mask = sys::sigprocmask(SIG_SETMASK, 0);
    c.check(
        mask == Ok(bit(SIGHUP)),
        format_args!("the signal mask is kept: {:?}", mask),
    );
    let mut buf = [0u8; 32];
    let n = sys::read(fd.unwrap_or(0), &mut buf).unwrap_or(0);
    c.check(
        &buf[..n] == EXEC_FILE_TEXT,
        format_args!("the open descriptor is kept"),
    );
    let grant = sys::device_grant(device::CONSOLE);
    c.check(
        grant == Err(Errno(EPERM)),
        format_args!("no privileges: device_grant fails: {:?}", grant),
    );
    // The dispositions, by what the signals do to a child that inherits them:
    // the handler of SIGUSR1 is gone (it would run in the new image, and the
    // child would survive), SIGUSR2 is still ignored.
    for (sig, expected, what) in [
        (
            SIGUSR1,
            killed(SIGUSR1),
            "caught SIGUSR1 is back to its default action",
        ),
        (
            SIGUSR2,
            exited(CHILD_OK),
            "ignored SIGUSR2 is still ignored",
        ),
    ] {
        let Some(child) = fork_child(c, || {
            let _ = sys::kill(sys::getpid() as i64, sig);
            CHILD_OK
        }) else {
            return;
        };
        let r = sys::waitpid(child as i64, 0);
        c.check(
            r == Ok((child, expected)),
            format_args!("{}: {:?}", what, r),
        );
    }
}

/// `exec-loop N [PID]`: `execve`s itself `N` more times, then checks that its
/// pid is still `PID` (the pid of the first one).
fn exec_loop(c: &mut Checks, n: u64, pid: Option<u64>) {
    let pid = pid.unwrap_or_else(sys::getpid);
    if n == 0 {
        let now = sys::getpid();
        c.check(
            now == pid,
            format_args!("the pid survived every execve: {}", now),
        );
        return;
    }
    let (mut n_buf, mut pid_buf) = ([0u8; 20], [0u8; 20]);
    let argv = [
        "probe_test",
        "exec-loop",
        decimal(n - 1, &mut n_buf),
        decimal(pid, &mut pid_buf),
    ];
    let e = sys::execve(SELF, &argv, &[]);
    c.check(
        false,
        format_args!("execve, {} from the end: errno {}", n, e.0),
    );
}
