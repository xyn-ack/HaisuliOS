// SPDX-License-Identifier: GPL-2.0

//! `smp_test`: a test program for multiple CPUs and threads.
//!
//! Exercises what the shell cannot reach of SMP and threads, for the tests (`test/test_smp.py`),
//! with the runtime's threads ([`haisuli_rt::thread`]). It was written before the kernel had
//! either (test first).
//!
//! `smp_test CASE [N]` runs one case. It prints a line for every check, `ok` or `FAIL`, and then
//! `CASE: passed` or `CASE: failed`; it exits with status 0 only if every check passed. `N` is the
//! number of CPUs of the machine, except where it says otherwise. The cases:
//!
//! - `spin`: prints `smp_test: spinning` and then keeps its CPU busy forever, without system
//!   calls. (Not a check: a job for the shell tests to run next to others.)
//! - `threads`: four threads run, each sees its own thread id and the process's pid, and the
//!   process goes on once they have ended. Needs only one CPU.
//! - `parallel N`: `N` threads (the main thread and `N - 1` more) are running at the same time, on
//!   `N` different CPUs.
//! - `thread-loop N`: starts and ends `N` threads, one after another: a leak would run out.
//! - `syscalls N`: `N` threads make system calls and process server requests at the same time.
//! - `exit N`: a process whose threads keep `N - 1` CPUs busy exits: that ends them all.
//! - `kill N`: the same process is killed by `SIGKILL` instead.
//! - `fault N`: a thread of a process that keeps every CPU busy faults: the whole process dies of
//!   `SIGSEGV`.
//! - `exec N`: the same process runs `execve`: that ends its other threads.
//! - `fork`: a child forked by a process with a running thread has only the thread that forked.
//! - `signals`: a signal sent to the process ends the `pause` of the thread that waits in it, and
//!   its handler runs in that thread.
//!
//! After `exit`, `kill`, `fault` and `exec`, the case checks that the threads left every CPU by
//! running `parallel N`: if one still spins, that never finishes.

#![no_std]
#![no_main]
// The only unsafe code makes the page fault of the `fault` case.
#![deny(unsafe_code)]

use core::fmt;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use haisuli_rt::abi::errno::EINTR;
use haisuli_rt::abi::fcntl::{O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};
use haisuli_rt::abi::signal::{SIGKILL, SIGSEGV, SIGUSR1};
use haisuli_rt::abi::Termination;
use haisuli_rt::println;
use haisuli_rt::sys::{self, Errno, SigHandler, WaitStatus};
use haisuli_rt::thread::{spawn, Thread};

/// The most threads a case starts at once (besides the main thread).
const MAX_THREADS: usize = 8;

haisuli_rt::entry!(main);

/// Exit status of a child whose own checks passed (and failed).
const CHILD_OK: i32 = 0;
const CHILD_FAILED: i32 = 1;
/// The exit status of the process that the `exit` case ends.
const EXIT_CODE: i32 = 3;
/// The file through which the child of the `kill` case says that its threads are spinning.
const READY_FILE: &str = "/smp_test.ready";
/// How many times a waiting loop yields before it gives up.
const PATIENCE: u32 = 100_000;
/// Rounds of system calls that each thread of the `syscalls` case makes.
const ROUNDS: u64 = 200;
/// How many threads the `threads` case starts.
const THREADS: usize = 4;
/// What a thread records in place of a value that a system call failed to give.
const NONE: u64 = u64::MAX;
/// How many times `parallel` lets its threads meet before it concludes they share a CPU.
const MEETINGS: u32 = 10;
/// How many times `parallel` yields between meetings, to let slow CPUs (emulated ones, on a busy
/// host) catch up.
const BETWEEN_MEETINGS: u32 = 1000;

fn main(info: haisuli_rt::StartInfo) -> i32 {
    let mut args = info.args().skip(1);
    let Some(case) = args.next() else {
        println!("usage: smp_test CASE [N] (see the source)");
        return 2;
    };
    let n = args.next().and_then(|s| s.parse::<usize>().ok());
    let mut c = Checks::new();
    match (case, n) {
        ("spin", None) => spin_forever(),
        ("threads", None) => threads(&mut c),
        ("fork", None) => fork(&mut c),
        ("signals", None) => signals(&mut c),
        ("thread-loop", Some(n)) => thread_loop(&mut c, n),
        ("parallel" | "syscalls" | "exit" | "kill" | "fault" | "exec", Some(n))
            if (1..=MAX_THREADS + 1).contains(&n) =>
        {
            match case {
                "parallel" => parallel(&mut c, n),
                "syscalls" => syscalls(&mut c, n),
                "exit" | "kill" | "fault" | "exec" if n < 2 => {
                    println!("smp_test {}: needs N >= 2", case);
                    return 2;
                }
                "exit" => exit(&mut c, n),
                "kill" => kill(&mut c, n),
                "fault" => fault(&mut c, n),
                _ => exec(&mut c, n),
            }
        }
        _ => {
            println!(
                "smp_test: bad case or N (1 to {}): {}",
                MAX_THREADS + 1,
                case
            );
            return 2;
        }
    }
    c.finish(case)
}

/// The checks of a case, and whether one failed.
struct Checks {
    failed: bool,
}

impl Checks {
    fn new() -> Checks {
        Checks { failed: false }
    }

    /// Reports one check; returns whether it passed.
    fn check(&mut self, ok: bool, what: fmt::Arguments<'_>) -> bool {
        println!("  {} {}", if ok { "ok  " } else { "FAIL" }, what);
        self.failed |= !ok;
        ok
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

/// The wait status of a process that exited with `code`.
fn exited(code: i32) -> WaitStatus {
    WaitStatus(Termination::exit(code).wait_status())
}

/// The wait status of a process that signal `sig` killed.
fn killed(sig: u64) -> WaitStatus {
    WaitStatus(Termination::Signalled(sig).wait_status())
}

// ---- jobs for threads ---------------------------------------------------------------------------

/// How many threads have started spinning (see [`spin`]).
static SPINNING: AtomicU32 = AtomicU32::new(0);
/// Incremented all the time by every spinning thread.
static SPINS: AtomicU64 = AtomicU64::new(0);

/// Keeps a CPU busy forever, without system calls.
extern "C" fn spin(_: u64) {
    SPINNING.fetch_add(1, Ordering::SeqCst);
    loop {
        SPINS.fetch_add(1, Ordering::Relaxed);
        spin_loop();
    }
}

/// `spin`: the main thread spins.
fn spin_forever() -> ! {
    println!("smp_test: spinning");
    loop {
        spin_loop();
    }
}

/// Starts `n` threads that spin forever and waits until they do.
fn spinners(c: &mut Checks, n: usize) -> bool {
    let before = SPINNING.load(Ordering::SeqCst);
    for i in 0..n {
        if let Err(e) = spawn(spin, 0) {
            return c.check(
                false,
                format_args!("start spinning thread {}: errno {}", i, e.0),
            );
        }
    }
    let all = before + n as u32;
    for _ in 0..PATIENCE {
        if SPINNING.load(Ordering::SeqCst) >= all {
            return c.check(true, format_args!("{} threads spin", n));
        }
        sys::sched_yield();
    }
    let running = SPINNING.load(Ordering::SeqCst) - before;
    c.check(
        false,
        format_args!("only {} of {} threads spin", running, n),
    )
}

/// A thread that does nothing.
extern "C" fn nothing(_: u64) {}

/// Writes to an unmapped address.
#[expect(unsafe_code)]
extern "C" fn page_fault(_: u64) {
    let address = core::ptr::without_provenance_mut::<u64>(8);
    // SAFETY: Not safe at all, on purpose: the first page is never mapped, so the CPU faults
    // before anything is written, and the process ends there.
    unsafe { core::ptr::write_volatile(address, 1) };
}

// ---- threads ------------------------------------------------------------------------------------

/// What each thread of the `threads` case saw: its thread id and the pid.
static SEEN_TID: [AtomicU64; THREADS] = [const { AtomicU64::new(0) }; THREADS];
static SEEN_PID: [AtomicU64; THREADS] = [const { AtomicU64::new(0) }; THREADS];
/// How many threads of the `threads` case ran.
static RAN: AtomicU32 = AtomicU32::new(0);

/// A thread of the `threads` case, number `i`.
extern "C" fn record(i: u64) {
    let i = i as usize;
    SEEN_TID[i].store(sys::gettid(), Ordering::SeqCst);
    SEEN_PID[i].store(sys::getpid(), Ordering::SeqCst);
    RAN.fetch_add(1, Ordering::SeqCst);
}

/// `threads`: a few threads, on any number of CPUs.
fn threads(c: &mut Checks) {
    let main_tid = sys::gettid();
    c.check(
        main_tid != 0,
        format_args!("gettid in the main thread: {}", main_tid),
    );
    let pid = sys::getpid();
    let mut started: [Option<Thread>; THREADS] = [const { None }; THREADS];
    for (i, slot) in started.iter_mut().enumerate() {
        match spawn(record, i as u64) {
            Ok(t) => *slot = Some(t),
            Err(e) => {
                c.check(false, format_args!("start thread {}: errno {}", i, e.0));
            }
        }
    }
    let mut tids = [0; THREADS];
    for (i, t) in started.into_iter().enumerate() {
        if let Some(t) = t {
            tids[i] = t.id();
            t.join();
        }
    }
    if c.failed {
        return;
    }
    c.check(true, format_args!("{} threads started and ended", THREADS));
    let ran = RAN.load(Ordering::SeqCst);
    c.check(
        ran == THREADS as u32,
        format_args!("every thread ran, in the same memory: {}", ran),
    );
    let seen: [u64; THREADS] = core::array::from_fn(|i| SEEN_TID[i].load(Ordering::SeqCst));
    c.check(
        seen == tids,
        format_args!(
            "gettid in each thread is the id that clone returned: {:?} {:?}",
            seen, tids
        ),
    );
    let distinct = (0..THREADS).all(|i| (0..i).all(|j| tids[i] != tids[j]));
    c.check(
        distinct && !tids.contains(&main_tid),
        format_args!(
            "every thread has an id of its own: {:?}, main {}",
            tids, main_tid
        ),
    );
    let pids: [u64; THREADS] = core::array::from_fn(|i| SEEN_PID[i].load(Ordering::SeqCst));
    c.check(
        pids.iter().all(|&p| p == pid),
        format_args!(
            "getpid in every thread is the process's pid {}: {:?}",
            pid, pids
        ),
    );
    c.check(
        sys::getpid() == pid,
        format_args!("the process goes on after its threads ended"),
    );
}

// ---- parallel -----------------------------------------------------------------------------------

/// How many threads the `parallel` barrier waits for.
static PARTIES: AtomicU32 = AtomicU32::new(0);
/// Threads that arrived at the barrier, and that recorded their CPU.
static ARRIVED: AtomicU32 = AtomicU32::new(0);
static RECORDED: AtomicU32 = AtomicU32::new(0);
/// The CPU each thread ran on, while all of them were running.
static CPU_OF: [AtomicU64; MAX_THREADS + 1] = [const { AtomicU64::new(NONE) }; MAX_THREADS + 1];

/// Spins, without system calls, until `count` reaches [`PARTIES`].
fn wait_for_all(count: &AtomicU32) {
    let parties = PARTIES.load(Ordering::SeqCst);
    while count.load(Ordering::SeqCst) < parties {
        spin_loop();
    }
}

/// Thread `i` of `parallel`.
///
/// Nobody leaves the barrier before every thread has recorded its CPU, so they all record it
/// while all of them are running. Spinning threads only run at the same time on different CPUs.
extern "C" fn meet(i: u64) {
    ARRIVED.fetch_add(1, Ordering::SeqCst);
    wait_for_all(&ARRIVED);
    CPU_OF[i as usize].store(sys::getcpu().unwrap_or(NONE), Ordering::SeqCst);
    RECORDED.fetch_add(1, Ordering::SeqCst);
    wait_for_all(&RECORDED);
}

/// `parallel N`: the main thread and `n - 1` more spin at the same time, on `n` CPUs.
///
/// A preemptive scheduler may briefly run two of them on one CPU (while another CPU is still
/// waking up), so this makes up to [`MEETINGS`] attempts; a CPU that never takes part (one that
/// still runs something else, or no SMP at all) fails each.
fn parallel(c: &mut Checks, n: usize) {
    for attempt in 1..=MEETINGS {
        let Some(cpus) = meeting(c, n) else {
            return;
        };
        let cpus = &cpus[..n];
        if !c.check(
            cpus.iter().all(|&cpu| cpu < n as u64),
            format_args!("getcpu gives a CPU from 0 to {}: {:?}", n - 1, cpus),
        ) {
            return;
        }
        let distinct = (0..n).all(|i| (0..i).all(|j| cpus[i] != cpus[j]));
        if distinct || attempt == MEETINGS {
            c.check(
                distinct,
                format_args!(
                    "{} threads ran at the same time, on different CPUs: {:?}",
                    n, cpus
                ),
            );
            return;
        }
        println!("  (attempt {}: CPUs {:?}; again)", attempt, cpus);
        for _ in 0..BETWEEN_MEETINGS {
            sys::sched_yield();
        }
    }
}

/// One meeting of `n` threads at the barrier of [`meet`]; returns the CPU of each.
fn meeting(c: &mut Checks, n: usize) -> Option<[u64; MAX_THREADS + 1]> {
    PARTIES.store(n as u32, Ordering::SeqCst);
    ARRIVED.store(0, Ordering::SeqCst);
    RECORDED.store(0, Ordering::SeqCst);
    for cpu in &CPU_OF {
        cpu.store(NONE, Ordering::SeqCst);
    }
    let mut started: [Option<Thread>; MAX_THREADS] = [const { None }; MAX_THREADS];
    for (i, slot) in started.iter_mut().enumerate().take(n - 1) {
        match spawn(meet, i as u64 + 1) {
            Ok(t) => *slot = Some(t),
            Err(e) => {
                // The others would wait for this one forever: they stay behind, spinning.
                c.check(false, format_args!("start thread {}: errno {}", i + 1, e.0));
                return None;
            }
        }
    }
    meet(0);
    for t in started.into_iter().flatten() {
        t.join();
    }
    Some(core::array::from_fn(|i| CPU_OF[i].load(Ordering::SeqCst)))
}

// ---- thread-loop --------------------------------------------------------------------------------

/// `thread-loop N`: `n` threads, one after another.
fn thread_loop(c: &mut Checks, n: usize) {
    for i in 0..n {
        match spawn(nothing, 0) {
            Ok(t) => t.join(),
            Err(e) => {
                c.check(
                    false,
                    format_args!("start thread {} of {}: errno {}", i + 1, n, e.0),
                );
                return;
            }
        }
    }
    c.check(
        true,
        format_args!("{} threads started and ended, one after another", n),
    );
}

// ---- syscalls -----------------------------------------------------------------------------------

/// The pid that every thread of the `syscalls` case must see.
static PID: AtomicU64 = AtomicU64::new(0);
/// Wrong answers to the system calls of the `syscalls` case, and rounds completed.
static WRONG: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicU64 = AtomicU64::new(0);

/// A thread of `syscalls`: a system call to the kernel and a request to the process server per
/// round; lets the others run in between.
extern "C" fn make_calls(_: u64) {
    let tid = sys::gettid();
    for _ in 0..ROUNDS {
        if sys::gettid() != tid || tid == 0 || sys::getpid() != PID.load(Ordering::SeqCst) {
            WRONG.fetch_add(1, Ordering::SeqCst);
        }
        sys::sched_yield();
        DONE.fetch_add(1, Ordering::SeqCst);
    }
}

/// `syscalls N`: `n` threads (the main one too) make system calls at the same time.
fn syscalls(c: &mut Checks, n: usize) {
    PID.store(sys::getpid(), Ordering::SeqCst);
    let mut started: [Option<Thread>; MAX_THREADS] = [const { None }; MAX_THREADS];
    for (i, slot) in started.iter_mut().enumerate().take(n - 1) {
        match spawn(make_calls, 0) {
            Ok(t) => *slot = Some(t),
            Err(e) => {
                c.check(false, format_args!("start thread {}: errno {}", i + 1, e.0));
            }
        }
    }
    make_calls(0);
    for t in started.into_iter().flatten() {
        t.join();
    }
    if c.failed {
        return;
    }
    let done = DONE.load(Ordering::SeqCst);
    c.check(
        done == n as u64 * ROUNDS,
        format_args!("{} threads made {} rounds of calls: {}", n, ROUNDS, done),
    );
    let wrong = WRONG.load(Ordering::SeqCst);
    c.check(
        wrong == 0,
        format_args!(
            "gettid and getpid always gave the caller's ids: {} wrong",
            wrong
        ),
    );
}

// ---- the end of a process with threads ----------------------------------------------------------

/// Checks that no thread of an ended process still runs, with `parallel`: that needs all `n`
/// CPUs, so it never finishes if one still spins.
fn cpus_are_free(c: &mut Checks, n: usize) {
    if !c.failed {
        println!("  (every CPU must be free again)");
        parallel(c, n);
    }
}

/// `exit N`: the process exits while `n - 1` of its threads spin.
fn exit(c: &mut Checks, n: usize) {
    let Some(child) = fork_child(c, || {
        let mut c = Checks::new();
        if !spinners(&mut c, n - 1) {
            return CHILD_FAILED;
        }
        EXIT_CODE
    }) else {
        return;
    };
    let r = sys::waitpid(child as i64, 0);
    c.check(
        r == Ok((child, exited(EXIT_CODE))),
        format_args!("the process exited with status {}: {:?}", EXIT_CODE, r),
    );
    cpus_are_free(c, n);
}

/// `kill N`: the process is killed while `n - 1` of its threads spin (and the main thread sleeps
/// in `pause`).
fn kill(c: &mut Checks, n: usize) {
    let created = sys::open(READY_FILE, O_CREAT | O_WRONLY | O_TRUNC).and_then(sys::close);
    if !c.check(
        created.is_ok(),
        format_args!("create {}: {:?}", READY_FILE, created),
    ) {
        return;
    }
    let Some(child) = fork_child(c, || {
        let mut c = Checks::new();
        let ok = spinners(&mut c, n - 1);
        if let Ok(fd) = sys::open(READY_FILE, O_WRONLY | O_TRUNC) {
            let _ = sys::write(fd, if ok { b"ok" } else { b"no" });
            let _ = sys::close(fd);
        }
        if !ok {
            return CHILD_FAILED;
        }
        loop {
            let _ = sys::pause();
        }
    }) else {
        return;
    };
    let mut ready = [0u8; 2];
    for _ in 0..PATIENCE {
        if let Ok(fd) = sys::open(READY_FILE, O_RDONLY) {
            let n = sys::read(fd, &mut ready).unwrap_or(0);
            let _ = sys::close(fd);
            if n == ready.len() {
                break;
            }
        }
        sys::sched_yield();
    }
    if c.check(&ready == b"ok", format_args!("the child's threads spin")) {
        let sent = sys::kill(child as i64, SIGKILL);
        c.check(sent.is_ok(), format_args!("SIGKILL the child: {:?}", sent));
    }
    let r = sys::waitpid(child as i64, 0);
    c.check(
        r == Ok((child, killed(SIGKILL))),
        format_args!("the child died of SIGKILL: {:?}", r),
    );
    cpus_are_free(c, n);
}

/// `fault N`: while the main thread and `n - 2` more spin, one more thread faults.
fn fault(c: &mut Checks, n: usize) {
    let Some(child) = fork_child(c, || {
        let mut c = Checks::new();
        if !spinners(&mut c, n - 2) {
            return CHILD_FAILED;
        }
        if let Err(e) = spawn(page_fault, 0) {
            c.check(
                false,
                format_args!("start the faulting thread: errno {}", e.0),
            );
            return CHILD_FAILED;
        }
        spin(0);
        CHILD_FAILED
    }) else {
        return;
    };
    let r = sys::waitpid(child as i64, 0);
    c.check(
        r == Ok((child, killed(SIGSEGV))),
        format_args!("the whole process died of SIGSEGV: {:?}", r),
    );
    cpus_are_free(c, n);
}

/// `exec N`: the process runs `/bin/true` while `n - 1` of its threads spin.
fn exec(c: &mut Checks, n: usize) {
    let Some(child) = fork_child(c, || {
        let mut c = Checks::new();
        if !spinners(&mut c, n - 1) {
            return CHILD_FAILED;
        }
        let e = sys::execve("/bin/true", &["true"], &[]);
        c.check(false, format_args!("execve /bin/true: errno {}", e.0));
        CHILD_FAILED
    }) else {
        return;
    };
    let r = sys::waitpid(child as i64, 0);
    c.check(
        r == Ok((child, exited(0))),
        format_args!("the process ran /bin/true: {:?}", r),
    );
    cpus_are_free(c, n);
}

// ---- fork ---------------------------------------------------------------------------------------

/// `fork`: a child of a process with a spinning thread.
fn fork(c: &mut Checks) {
    if !spinners(c, 1) {
        return;
    }
    let Some(child) = fork_child(c, || {
        // A copy of the spinning thread would make `SPINS` go up.
        let before = SPINS.load(Ordering::SeqCst);
        for _ in 0..100 {
            sys::sched_yield();
        }
        for _ in 0..1_000_000 {
            spin_loop();
        }
        if SPINS.load(Ordering::SeqCst) == before {
            CHILD_OK
        } else {
            CHILD_FAILED
        }
    }) else {
        return;
    };
    let r = sys::waitpid(child as i64, 0);
    c.check(
        r == Ok((child, exited(CHILD_OK))),
        format_args!("the child has only the thread that forked: {:?}", r),
    );
    // Another CPU runs it, so it may take a moment to be seen: wait for it, patiently.
    let before = SPINS.load(Ordering::SeqCst);
    let mut after = before;
    for _ in 0..PATIENCE {
        after = SPINS.load(Ordering::SeqCst);
        if after != before {
            break;
        }
        sys::sched_yield();
    }
    c.check(
        after > before,
        format_args!("the parent's thread still runs: {} -> {}", before, after),
    );
}

// ---- signals ------------------------------------------------------------------------------------

/// The thread in which the SIGUSR1 handler of the `signals` case ran last, and how often it ran.
static HANDLED_IN: AtomicU64 = AtomicU64::new(0);
static HANDLED: AtomicU32 = AtomicU32::new(0);
/// What the pausing thread of the `signals` case found: whether its `pause` ended with `EINTR`,
/// and in which thread the handler had run by then (0 until it is done).
static PAUSE_EINTR: AtomicU32 = AtomicU32::new(0);
static PAUSE_SAW: AtomicU64 = AtomicU64::new(0);

extern "C" fn on_usr1(_: i32) {
    HANDLED_IN.store(sys::gettid(), Ordering::SeqCst);
    HANDLED.fetch_add(1, Ordering::SeqCst);
}

/// The thread of the `signals` case that pauses.
extern "C" fn pauser(_: u64) {
    let e = sys::pause();
    PAUSE_EINTR.store(u32::from(e == Errno(EINTR)), Ordering::SeqCst);
    PAUSE_SAW.store(HANDLED_IN.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// `signals`: a thread pauses; the main thread signals the process until the pause ends.
///
/// A signal that comes before the thread waits may run its handler in the main thread (as
/// POSIX allows); once the thread waits in `pause`, the signal must end it, with the handler
/// run in that thread.
fn signals(c: &mut Checks) {
    let set = sys::sigaction(SIGUSR1, SigHandler::Catch(on_usr1), 0, 0);
    if !c.check(set.is_ok(), format_args!("catch SIGUSR1: {:?}", set)) {
        return;
    }
    let t = match spawn(pauser, 0) {
        Ok(t) => t,
        Err(e) => {
            c.check(
                false,
                format_args!("start the pausing thread: errno {}", e.0),
            );
            return;
        }
    };
    let me = sys::getpid() as i64;
    let mut sent = 0;
    for _ in 0..PATIENCE {
        if PAUSE_SAW.load(Ordering::SeqCst) != 0 {
            break;
        }
        if sys::kill(me, SIGUSR1).is_ok() {
            sent += 1;
        }
        for _ in 0..100 {
            sys::sched_yield();
        }
    }
    let tid = t.id();
    t.join();
    let saw = PAUSE_SAW.load(Ordering::SeqCst);
    c.check(
        PAUSE_EINTR.load(Ordering::SeqCst) == 1,
        format_args!(
            "the thread's pause ended with EINTR ({} signals sent)",
            sent
        ),
    );
    c.check(
        saw == tid,
        format_args!(
            "the handler that ended it ran in that thread: {} (the thread is {})",
            saw, tid
        ),
    );
    // Signals do not queue: one sent while the last is still pending merges with it.
    let handled = HANDLED.load(Ordering::SeqCst);
    c.check(
        (1..=sent).contains(&handled),
        format_args!(
            "the handler ran, at most once per signal: {} of {}",
            handled, sent
        ),
    );
}
