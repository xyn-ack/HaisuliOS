// SPDX-License-Identifier: GPL-2.0

//! Upcalls.
//!
//! A request to make a task enter a handler, the mechanism under POSIX signal handlers.
//!
//! The kernel knows nothing about signals: which ones exist, what they do by default, which are
//! blocked, all of that is the process server's business. What it provides is the one thing only
//! it can do: when the task manager asks for an upcall (`TASK_UPCALL`), the kernel saves the
//! thread's registers in a frame on the user stack on its way back to user mode, and enters the
//! handler; `rt_sigreturn` restores them from the frame. The frame's layout is the
//! architecture's ([`crate::arch::upcall`]): Linux's signal frame, so that code written for
//! Linux signal handlers, such as a future libc port, finds things where it expects them.

use crate::vmm::USER_LIMIT;
use haisuli_abi::upcall::{RESTART, WORDS};

/// An upcall that the task manager requested (see [`haisuli_abi::upcall`]).
#[derive(Clone, Copy)]
pub(crate) struct Upcall {
    /// Where the handler starts.
    pub(crate) handler: u64,
    /// Where the handler returns to; it must make the `rt_sigreturn` call.
    pub(crate) restorer: u64,
    /// The handler's first argument, and `siginfo.si_signo`.
    pub(crate) arg: u64,
    /// `siginfo.si_pid`.
    pub(crate) sender: u64,
    /// The frame's `uc_sigmask`, which the kernel does not interpret.
    pub(crate) mask: u64,
    /// Whether to restart a system call that failed with `-EINTR`.
    pub(crate) restart: bool,
}

impl Upcall {
    /// Parses a request; `None` if the handler or the restorer is not a user address.
    pub(crate) fn from_words(w: &[u64; WORDS]) -> Option<Upcall> {
        let [handler, restorer, arg, sender, mask, flags] = *w;
        (is_user_address(handler) && is_user_address(restorer)).then_some(Upcall {
            handler,
            restorer,
            arg,
            sender,
            mask,
            restart: flags & RESTART != 0,
        })
    }
}

/// Whether `addr` is in the user half of the address space.
///
/// Anything else must never become a thread's program counter or stack pointer on the way to
/// user mode.
pub(crate) fn is_user_address(addr: u64) -> bool {
    addr < USER_LIMIT
}
