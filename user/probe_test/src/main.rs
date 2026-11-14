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
//! - `exec`: what `execve` keeps and what it resets, and that a failed `execve` returns.
//! - `exec-loop N`: `N` `execve`s in a row: a leak would run out of memory.

#![no_std]
#![no_main]
// The only unsafe code makes the page fault of the `fault` case.
#![deny(unsafe_code)]

use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};
use haisuli_rt::abi::errno::{E2BIG, EACCES, EINTR, ENOENT, ENOEXEC, EPERM};
use haisuli_rt::abi::fcntl::{O_CREAT, O_RDONLY, O_RDWR, O_TRUNC};
use haisuli_rt::abi::signal::{
    bit, NSIG, SA_RESTART, SIGHUP, SIGSEGV, SIGUSR1, SIGUSR2, SIG_BLOCK, SIG_SETMASK,
};
use haisuli_rt::abi::Termination;
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
        yield_a_while(); // The parent waits meanwhile.
        let _ = sys::kill(parent as i64, SIGUSR1);
        yield_a_while();
        5
    }) else {
        return;
    };
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
    let io = sys::ioperm(0x3f8, 8, true);
    c.check(
        io == Err(Errno(EPERM)),
        format_args!("no privileges: ioperm fails: {:?}", io),
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
