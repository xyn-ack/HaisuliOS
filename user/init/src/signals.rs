// SPDX-License-Identifier: GPL-2.0

//! Signal names and the `kill` command.
//!
//! Translates between signal numbers, names and descriptions, and implements `kill`.

use haisuli_rt::abi::errno;
use haisuli_rt::abi::signal::{NAMES, NSIG};
use haisuli_rt::{print, println, sys};

/// The signal called `name` (`TERM`, `SIGTERM` or `term`), or with number `name` (0 to 31).
pub(crate) fn parse(name: &str) -> Option<u32> {
    if let Ok(n) = name.parse::<u32>() {
        return (n < NSIG).then_some(n);
    }
    let bare = match name.get(..3) {
        Some(p) if p.eq_ignore_ascii_case("SIG") => &name[3..],
        _ => name,
    };
    (1..NSIG).find(|&s| NAMES[s as usize].eq_ignore_ascii_case(bare))
}

/// What a job that signal `sig` terminated or stopped is reported as (like `strsignal`).
pub(crate) fn describe(sig: u32) -> &'static str {
    match sig {
        1 => "Hangup",
        2 => "Interrupt",
        3 => "Quit",
        4 => "Illegal instruction",
        5 => "Trace/breakpoint trap",
        6 => "Aborted",
        7 => "Bus error",
        8 => "Floating point exception",
        9 => "Killed",
        10 => "User defined signal 1",
        11 => "Segmentation fault",
        12 => "User defined signal 2",
        13 => "Broken pipe",
        14 => "Alarm clock",
        15 => "Terminated",
        19..=22 => "Stopped",
        _ => "Signal",
    }
}

/// The message for error number `e` (like `strerror`).
pub(crate) fn strerror(e: sys::Errno) -> &'static str {
    match e.0 {
        errno::EPERM => "Operation not permitted",
        errno::ESRCH => "No such process",
        errno::EINTR => "Interrupted system call",
        errno::ECHILD => "No child processes",
        errno::EAGAIN => "Resource temporarily unavailable",
        errno::ENOMEM => "Cannot allocate memory",
        errno::EINVAL => "Invalid argument",
        errno::ENOTTY => "Inappropriate ioctl for device",
        _ => "Unknown error",
    }
}

/// `kill [-s sig | -sig] pid...` and `kill -l [status]`; returns the exit status.
///
/// A negative pid is a process group. The default signal is `SIGTERM`.
pub(crate) fn kill(args: &[&str]) -> i32 {
    const USAGE: &str = "kill: usage: kill [-s sig | -sig] pid... | kill -l [status]";
    let mut sig = 15;
    let mut rest = args;
    match rest.first() {
        Some(&"-l") => return list(&rest[1..]),
        Some(&"-s") => {
            let Some(name) = rest.get(1) else {
                println!("{}", USAGE);
                return 2;
            };
            match parse(name) {
                Some(s) => sig = s,
                None => return invalid(name),
            }
            rest = &rest[2..];
        }
        Some(&"--") => rest = &rest[1..],
        Some(opt) if opt.len() > 1 && opt.starts_with('-') => {
            match parse(&opt[1..]) {
                Some(s) => sig = s,
                None => return invalid(&opt[1..]),
            }
            rest = &rest[1..];
        }
        _ => {}
    }
    if rest.first() == Some(&"--") {
        rest = &rest[1..];
    }
    if rest.is_empty() {
        println!("{}", USAGE);
        return 2;
    }
    let mut status = 0;
    for arg in rest {
        let Ok(pid) = arg.parse::<i32>() else {
            println!("kill: {}: arguments must be process or job IDs", arg);
            status = 1;
            continue;
        };
        if let Err(e) = sys::kill(pid, sig) {
            println!("kill: ({}) - {}", pid, strerror(e));
            status = 1;
        }
    }
    status
}

fn invalid(name: &str) -> i32 {
    println!("kill: {}: invalid signal specification", name);
    1
}

/// `kill -l`: the signal names; `kill -l status`: the signal that a status stands for.
fn list(args: &[&str]) -> i32 {
    let Some(arg) = args.first() else {
        for sig in 1..NSIG {
            print!(
                "{}{}",
                NAMES[sig as usize],
                if sig % 8 == 0 { "\n" } else { " " }
            );
        }
        println!();
        return 0;
    };
    let sig = match arg.parse::<u32>() {
        Ok(n) if n > 128 => Some(n - 128),
        Ok(n) => Some(n),
        Err(_) => parse(arg),
    };
    match sig {
        Some(s) if (1..NSIG).contains(&s) && arg.parse::<u32>().is_ok() => {
            println!("{}", NAMES[s as usize]);
            0
        }
        Some(s) if (1..NSIG).contains(&s) => {
            println!("{}", s);
            0
        }
        _ => invalid(arg),
    }
}
