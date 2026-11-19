// SPDX-License-Identifier: MIT

//! Traps.
//!
//! The shell's signal handling: its own signal policy, and the commands that `trap` attaches to
//! signals.
//!
//! Every signal the shell catches goes to one real signal handler, [`on_signal`], which only
//! records the signal in an atomic set: a handler interrupts the shell at an arbitrary point, so
//! it must not do more. The shell then runs the trap commands at safe points, between commands.
//! A caught signal also interrupts a blocking `read` or `wait` (no `SA_RESTART`), which is how
//! Ctrl-C at the prompt, or during `wait`, gets back to the shell's loop.
//!
//! Without a trap, an interactive shell catches SIGINT (to abandon the line being typed) and
//! ignores the other signals of a terminal's job control, which belong to the foreground job.

use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_rt::abi::signal::{
    self, NSIG, SIGINT, SIGKILL, SIGQUIT, SIGSTOP, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU,
};
use haisuli_rt::println;
use haisuli_rt::sys::{self, SigHandler};

/// Longest trap command.
const TRAP_MAX: usize = 128;

/// Signals caught since the shell last looked, as a signal set.
static CAUGHT: AtomicU64 = AtomicU64::new(0);

/// The handler of every signal the shell catches.
extern "C" fn on_signal(sig: i32) {
    CAUGHT.fetch_or(signal::bit(sig as u64), Ordering::Relaxed);
}

/// Whether any signal has been caught and not yet handled with [`take_caught`].
pub(crate) fn any_caught() -> bool {
    CAUGHT.load(Ordering::Relaxed) != 0
}

/// Whether `sig` has been caught and not yet handled.
pub(crate) fn caught(sig: u64) -> bool {
    CAUGHT.load(Ordering::Relaxed) & signal::bit(sig) != 0
}

/// The lowest-numbered signal caught and not yet handled.
pub(crate) fn first_caught() -> Option<u64> {
    let set = CAUGHT.load(Ordering::Relaxed);
    (set != 0).then(|| set.trailing_zeros() as u64 + 1)
}

/// Returns the caught signals (as a signal set) and forgets them.
pub(crate) fn take_caught() -> u64 {
    CAUGHT.swap(0, Ordering::Relaxed)
}

/// What `trap` made of a signal.
#[derive(Clone, Copy)]
enum Trap {
    /// No trap: the shell's own policy ([`shell_handler`]).
    None,
    /// `trap '' SIG`: ignored, also by the shell's jobs.
    Ignore,
    /// `trap 'command' SIG`: the command runs when the signal is caught.
    Command([u8; TRAP_MAX], usize),
}

/// What the shell itself does with `sig` when no trap is set.
fn shell_handler(sig: u64) -> SigHandler {
    match sig {
        SIGINT => SigHandler::Catch(on_signal),
        SIGQUIT | SIGTERM | SIGTSTP | SIGTTIN | SIGTTOU => SigHandler::Ignore,
        _ => SigHandler::Default,
    }
}

/// Sets the handler of `sig`: caught signals interrupt blocking calls.
fn install(sig: u64, handler: SigHandler) -> sys::Result<()> {
    sys::sigaction(sig, handler, 0, 0).map(drop)
}

/// The traps of the shell, by signal number.
pub(crate) struct Traps {
    traps: [Trap; NSIG as usize],
}

impl Traps {
    /// Installs the shell's signal policy, with no traps set.
    pub(crate) fn new() -> Traps {
        for sig in [SIGINT, SIGQUIT, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU] {
            let _ = install(sig, shell_handler(sig));
        }
        Traps {
            traps: [Trap::None; NSIG as usize],
        }
    }

    /// `trap action sig`: `-` restores the shell's policy, `''` ignores the
    /// signal, anything else is a command to run when it arrives.
    pub(crate) fn set(&mut self, sig: u64, action: &str) -> Result<(), &'static str> {
        if sig == SIGKILL || sig == SIGSTOP {
            return Err("cannot be trapped");
        }
        let (trap, handler) = match action {
            "-" => (Trap::None, shell_handler(sig)),
            "" => (Trap::Ignore, SigHandler::Ignore),
            command => {
                if command.len() > TRAP_MAX {
                    return Err("command too long");
                }
                let mut buf = [0u8; TRAP_MAX];
                buf[..command.len()].copy_from_slice(command.as_bytes());
                (
                    Trap::Command(buf, command.len()),
                    SigHandler::Catch(on_signal),
                )
            }
        };
        install(sig, handler).map_err(|_| "cannot set the handler")?;
        self.traps[sig as usize] = trap;
        Ok(())
    }

    /// The command trapping `sig`, copied into `buf`, if there is one.
    ///
    /// (Copied, because running it needs the whole shell.)
    pub(crate) fn command<'b>(&self, sig: u64, buf: &'b mut [u8; TRAP_MAX]) -> Option<&'b str> {
        match self.traps.get(sig as usize) {
            Some(Trap::Command(command, len)) => {
                buf[..*len].copy_from_slice(&command[..*len]);
                core::str::from_utf8(&buf[..*len]).ok()
            }
            _ => None,
        }
    }

    /// A buffer for [`command`](Self::command).
    pub(crate) fn buffer() -> [u8; TRAP_MAX] {
        [0; TRAP_MAX]
    }

    /// Prints the traps that are set, the way `trap` without operands does.
    pub(crate) fn list(&self) {
        for (sig, trap) in self.traps.iter().enumerate() {
            let name = signal::NAMES[sig];
            match trap {
                Trap::None => {}
                Trap::Ignore => println!("trap -- '' {}", name),
                Trap::Command(command, len) => println!(
                    "trap -- '{}' {}",
                    core::str::from_utf8(&command[..*len]).unwrap_or("?"),
                    name
                ),
            }
        }
    }

    /// Resets the signal handling of a freshly forked job.
    ///
    /// As POSIX requires for a subshell, traps go back to the default action,
    /// except that ignored signals stay ignored; the shell's own policy does
    /// not apply to its jobs either.
    pub(crate) fn reset_for_job(&self) {
        for sig in 1..NSIG {
            if sig == SIGKILL || sig == SIGSTOP || matches!(self.traps[sig as usize], Trap::Ignore)
            {
                continue;
            }
            let _ = install(sig, SigHandler::Default);
        }
    }
}
