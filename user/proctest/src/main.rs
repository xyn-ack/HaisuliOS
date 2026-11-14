// SPDX-License-Identifier: GPL-2.0

//! Process server test probe.
//!
//! Reports which POSIX process calls the kernel still implements, and whether the runtime's ones
//! work.
//!
//! Only the tests of the `process-server` target boot this program, as an extra boot module (there
//! is no `exec`). It makes the Linux-numbered `getpid`, `getppid`, `wait4`, `fork` and `exit`
//! system calls directly: once the process server owns processes, the kernel knows nothing about
//! them and must answer `-ENOSYS`. Then it forks, waits and exits through the runtime
//! ([`haisuli_rt::sys`]), which must keep working whoever implements it.
//!
//! The probe only reports facts, one `[proctest] what = value` line each on the kernel debug log
//! (which does not depend on the console), and ends with `[proctest] done`. The tests in
//! `test/test_process_server.py` judge them.

#![no_std]
#![no_main]
// The only unsafe code is the raw system calls in `raw`.
#![deny(unsafe_code)]

use core::fmt::Write;
use haisuli_rt::abi::WNOHANG;
use haisuli_rt::io::{BufWriter, Sink};
use haisuli_rt::sys;

haisuli_rt::entry!(main);

/// The exit code of the child that calls the kernel's `exit` directly.
const RAW_EXIT_CODE: i32 = 7;
/// The exit code of the child that exits through the runtime.
const CHILD_EXIT_CODE: i32 = 42;
/// `wait4`'s "any child" pid.
const ANY_CHILD: i64 = -1;

#[expect(unsafe_code)]
mod raw {
    //! System calls made directly, bypassing the runtime's wrappers.
    use haisuli_rt::abi::nr;
    use haisuli_rt::sys;

    /// A system call that passes the kernel no memory.
    #[derive(Clone, Copy)]
    pub(crate) enum Call {
        GetPid,
        GetPpid,
        /// `wait4(pid, NULL, options, NULL)`.
        Wait4 {
            pid: i64,
            options: u64,
        },
        Fork,
        Exit(i32),
    }

    /// Makes `call` and returns what the kernel returned (a negative errno on failure).
    pub(crate) fn syscall(call: Call) -> i64 {
        let (n, a0, a1, a2) = match call {
            Call::GetPid => (nr::GETPID, 0, 0, 0),
            Call::GetPpid => (nr::GETPPID, 0, 0, 0),
            Call::Wait4 { pid, options } => (nr::WAIT4, pid as u64, 0, options),
            Call::Fork => (nr::FORK, 0, 0, 0),
            Call::Exit(code) => (nr::EXIT, code as u64, 0, 0),
        };
        // SAFETY: None of these calls takes a pointer (`wait4`'s status and
        // rusage pointers are null), so the kernel touches none of our memory.
        // A `fork` child gets a private copy of the address space.
        unsafe { sys::syscall(n, a0, a1, a2, 0, 0, 0) }
    }
}

/// Writes `[proctest] what = value` to the kernel debug log.
fn report(what: &str, value: impl core::fmt::Display) {
    let mut w = BufWriter::new(Sink::DebugLog);
    let _ = writeln!(w, "[proctest] {} = {}", what, value);
    w.flush();
}

/// Reports a runtime call's result: the value, or `error <errno>`.
fn report_result<T: core::fmt::Display>(what: &str, result: sys::Result<T>) {
    match result {
        Ok(v) => report(what, v),
        Err(e) => report(what, format_args!("error {}", e.0)),
    }
}

fn main(_: haisuli_rt::StartInfo) -> i32 {
    probe_kernel();
    probe_runtime();
    report("done", "yes");
    0
}

/// Asks the kernel directly for the POSIX process calls.
fn probe_kernel() {
    use raw::Call;
    report("kernel getpid", raw::syscall(Call::GetPid));
    report("kernel getppid", raw::syscall(Call::GetPpid));
    // Before any fork: a kernel that still had processes would say ECHILD.
    report(
        "kernel wait4",
        raw::syscall(Call::Wait4 {
            pid: ANY_CHILD,
            options: WNOHANG,
        }),
    );

    let forked = raw::syscall(Call::Fork);
    if forked == 0 {
        // The kernel still forks, and this is its child: it has nothing to do.
        sys::exit(0);
    }
    report("kernel fork", forked);
    if forked > 0 {
        let _ = sys::waitpid(forked, 0);
    }

    // `exit` ends the caller if it works, so try it in a child.
    match sys::fork() {
        Ok(0) => {
            // Only reached if the kernel did not end this process.
            report("kernel exit", raw::syscall(Call::Exit(RAW_EXIT_CODE)));
            sys::exit(0);
        }
        Ok(child) => match sys::waitpid(child as i64, 0) {
            Ok((_, status)) => report("kernel exit child status", format_args!("{:#x}", status.0)),
            Err(e) => report("kernel exit child status", format_args!("error {}", e.0)),
        },
        Err(e) => report("kernel exit child", format_args!("fork error {}", e.0)),
    }
}

/// Forks, exits and waits through the runtime, as every program does.
fn probe_runtime() {
    report("pid", sys::getpid());
    match sys::fork() {
        Ok(0) => {
            report("child getpid", sys::getpid());
            report("child getppid", sys::getppid());
            sys::exit(CHILD_EXIT_CODE);
        }
        Ok(child) => {
            report("fork", child);
            match sys::waitpid(child as i64, 0) {
                Ok((pid, status)) => {
                    report("wait pid", pid);
                    report("wait status", format_args!("{:#x}", status.0));
                }
                Err(e) => report("wait pid", format_args!("error {}", e.0)),
            }
            // The child has been reaped, so it is no longer a child.
            report_result(
                "second wait",
                sys::waitpid(child as i64, WNOHANG).map(|(p, _)| p),
            );
        }
        Err(e) => report("fork", format_args!("error {}", e.0)),
    }
}
