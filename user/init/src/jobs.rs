// SPDX-License-Identifier: GPL-2.0

//! Jobs.
//!
//! Starting commands as jobs in child processes, and keeping track of them for `wait`.
//!
//! There is no `exec` yet, so a job is a forked copy of the shell that runs one command and
//! exits with its status. Every job gets a process group of its own. A foreground job also gets
//! the terminal, so Ctrl-C interrupts the job and not the shell; the shell waits for it and
//! takes the terminal back. A background job (`cmd &`) runs while the shell reads the next
//! command.
//!
//! Like a POSIX shell, the shell remembers the status of every background job until `wait`
//! collects it, even when it was reported as done before a prompt.

use crate::signals::{self, strerror};
use core::sync::atomic::Ordering;
use haisuli_rt::abi::errno::EINTR;
use haisuli_rt::abi::signal::{
    bit, NAMES, SIGINT, SIGQUIT, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU, SIG_BLOCK, SIG_SETMASK,
};
use haisuli_rt::abi::wait::{self, WNOHANG, WUNTRACED};
use haisuli_rt::sys::{self, Errno, Handler};
use haisuli_rt::{print, println};

/// The signals whose actions the shell changes, and which a new job resets to the defaults.
pub(crate) const JOB_CONTROL_SIGNALS: [u32; 6] =
    [SIGINT, SIGQUIT, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU];

/// How many background jobs the shell keeps track of.
const MAX_JOBS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Stopped,
    /// Terminated with this wait status.
    Done(i32),
}

#[derive(Clone, Copy)]
struct Job {
    /// The job number (`[1]`).
    id: u32,
    pid: u64,
    state: State,
    /// Whether the shell has reported the job as done.
    reported: bool,
    name: [u8; 16],
}

impl Job {
    fn name(&self) -> &str {
        let n = self.name.iter().position(|&c| c == 0).unwrap_or(16);
        core::str::from_utf8(&self.name[..n]).unwrap_or("?")
    }

    /// Whether `jobs`-like listings still show the job (it keeps its number).
    fn listed(&self) -> bool {
        !(matches!(self.state, State::Done(_)) && self.reported)
    }
}

/// The exit status (`$?`) for a wait status.
pub(crate) fn exit_status(wstatus: i32) -> i32 {
    if let Some(code) = wait::exit_status(wstatus) {
        code
    } else if let Some(sig) = wait::term_signal(wstatus).or(wait::stop_signal(wstatus)) {
        128 + sig as i32
    } else {
        0
    }
}

/// The background jobs.
pub(crate) struct Jobs {
    jobs: [Option<Job>; MAX_JOBS],
}

/// What became of a command run in the foreground.
pub(crate) enum Started {
    /// Its exit status.
    Foreground(i32),
    /// The pid of the background job.
    Background(u64),
    /// Nothing: it could not be started.
    Failed,
}

impl Jobs {
    pub(crate) const fn new() -> Self {
        Jobs {
            jobs: [None; MAX_JOBS],
        }
    }

    /// Starts a job that runs `run` (whose result is the job's exit status).
    ///
    /// A background job is added to the table and announced as `[1] 3`. For a foreground job,
    /// waits until it terminates (or stops) and returns its exit status.
    pub(crate) fn start(
        &mut self,
        name: &str,
        background: bool,
        run: impl FnOnce() -> i32,
    ) -> Started {
        // Until the child has reset the signal actions that the shell changed, no such signal
        // may reach it: block them around the fork.
        let mask = JOB_CONTROL_SIGNALS.iter().fold(0, |m, &s| m | bit(s));
        let old_mask = sys::sigprocmask(SIG_BLOCK, mask).unwrap_or(0);
        let pid = match sys::fork() {
            Ok(0) => {
                sys::set_name(name);
                let _ = sys::setpgid(0, 0);
                if !background {
                    let _ = sys::tcsetpgrp(sys::getpid());
                }
                for sig in JOB_CONTROL_SIGNALS {
                    let _ = sys::sigaction(sig, Handler::Default, 0, 0);
                }
                let _ = sys::sigprocmask(SIG_SETMASK, old_mask);
                sys::exit(run());
            }
            Ok(pid) => pid,
            Err(e) => {
                let _ = sys::sigprocmask(SIG_SETMASK, old_mask);
                println!("init: fork: {}", strerror(e));
                return Started::Failed;
            }
        };
        // The child does the same, but this way it does not matter which of them runs first.
        let _ = sys::setpgid(pid as i32, pid as i32);
        if background {
            let id = self.add(pid, name, State::Running);
            let _ = sys::sigprocmask(SIG_SETMASK, old_mask);
            println!("[{}] {}", id, pid);
            return Started::Background(pid);
        }
        let _ = sys::tcsetpgrp(pid);
        let _ = sys::sigprocmask(SIG_SETMASK, old_mask);
        let wstatus = loop {
            match sys::waitpid(pid as i32, WUNTRACED) {
                Ok(Some((_, status))) => break status,
                Err(Errno(EINTR)) | Ok(None) => {}
                Err(_) => break 0,
            }
        };
        take_terminal();
        if wait::stop_signal(wstatus).is_some() {
            let id = self.add(pid, name, State::Stopped);
            println!("\n[{}] Stopped  {}", id, name);
        }
        Started::Foreground(exit_status(wstatus))
    }

    /// Adds a job and returns its number.
    fn add(&mut self, pid: u64, name: &str, state: State) -> u32 {
        let id = 1 + self
            .jobs
            .iter()
            .flatten()
            .filter(|j| j.listed())
            .map(|j| j.id)
            .max()
            .unwrap_or(0);
        let mut job = Job {
            id,
            pid,
            state,
            reported: false,
            name: [0; 16],
        };
        let n = name.len().min(15);
        job.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        // A full table forgets the oldest status that was already reported.
        let slot = match self.jobs.iter().position(Option::is_none) {
            Some(slot) => Some(slot),
            None => self
                .jobs
                .iter()
                .position(|j| j.is_some_and(|j| !j.listed())),
        };
        if let Some(slot) = slot {
            self.jobs[slot] = Some(job);
        }
        id
    }

    fn find(&mut self, pid: u64) -> Option<&mut Job> {
        self.jobs.iter_mut().flatten().find(|j| j.pid == pid)
    }

    /// Collects the jobs that terminated or stopped, and reports them.
    ///
    /// Called before every prompt.
    pub(crate) fn report(&mut self) {
        while let Ok(Some((pid, wstatus))) = sys::waitpid(-1, WNOHANG | WUNTRACED) {
            // Children the shell does not know (orphans handed to init) are just reaped.
            let Some(job) = self.find(pid) else {
                continue;
            };
            if let Some(sig) = wait::stop_signal(wstatus) {
                job.state = State::Stopped;
                println!(
                    "[{}] Stopped ({})  {}",
                    job.id,
                    NAMES[sig as usize],
                    job.name()
                );
            } else {
                job.state = State::Done(wstatus);
            }
        }
        for job in self.jobs.iter_mut().flatten() {
            if let (State::Done(wstatus), false) = (job.state, job.reported) {
                job.reported = true;
                print!("[{}] ", job.id);
                if let Some(code) = wait::exit_status(wstatus) {
                    if code == 0 {
                        print!("Done");
                    } else {
                        print!("Exit {}", code);
                    }
                } else if let Some(sig) = wait::term_signal(wstatus) {
                    print!("{}", signals::describe(sig));
                }
                println!("  {}", job.name());
            }
        }
    }

    /// `wait [pid...]`: waits for the given background jobs, or all of them; returns the exit
    /// status.
    ///
    /// The status is the last job's, or 127 if that is not a child of the shell (any more).
    /// Ctrl-C interrupts the wait (status 130).
    pub(crate) fn wait(&mut self, args: &[&str]) -> i32 {
        if args.is_empty() {
            for i in 0..MAX_JOBS {
                if let Some(job) = self.jobs[i] {
                    if let Err(status) = self.collect(job.pid) {
                        return status;
                    }
                }
                self.jobs[i] = None;
            }
            return 0;
        }
        let mut status = 0;
        for arg in args {
            let Ok(pid) = arg.parse::<u64>() else {
                println!("wait: `{}': not a pid", arg);
                status = 2;
                continue;
            };
            if self.find(pid).is_none() {
                println!("wait: pid {} is not a child of this shell", pid);
                status = 127;
                continue;
            }
            match self.collect(pid) {
                Ok(s) => status = s,
                Err(s) => return s,
            }
            if let Some(job) = self
                .jobs
                .iter_mut()
                .find(|j| j.is_some_and(|j| j.pid == pid))
            {
                *job = None;
            }
        }
        status
    }

    /// Waits until job `pid` has terminated and returns its exit status (`Err` if a signal
    /// interrupted the wait).
    fn collect(&mut self, pid: u64) -> Result<i32, i32> {
        if let Some(State::Done(wstatus)) = self.find(pid).map(|j| j.state) {
            return Ok(exit_status(wstatus));
        }
        if crate::INTERRUPTED.load(Ordering::Relaxed) {
            return Err(128 + SIGINT as i32); // Ctrl-C just before the wait.
        }
        match sys::waitpid(pid as i32, 0) {
            Ok(Some((_, wstatus))) => Ok(exit_status(wstatus)),
            Err(Errno(EINTR)) => Err(128 + SIGINT as i32),
            Ok(None) | Err(_) => Ok(127),
        }
    }
}

/// Makes the shell's process group the terminal's foreground process group (again).
pub(crate) fn take_terminal() {
    if let Ok(pgrp) = sys::getpgid(0) {
        let _ = sys::tcsetpgrp(pgrp);
    }
}
