// SPDX-License-Identifier: GPL-2.0

//! Signals.
//!
//! Signal numbers, dispositions, default actions and signal sets: a process's signal state.
//!
//! The process server decides everything about signals (see `Server::post` and `Server::act`);
//! the kernel only provides the upcall that enters a handler. This module holds what those
//! decisions need: each process's [`SigAction`]s, and [`SigSet`]s for the pending and blocked
//! signals.

use haisuli_rt::abi::signal::{
    self, NSIG, SA_NODEFER, SA_RESETHAND, SA_RESTORER, SIGCHLD, SIGCONT, SIGKILL, SIGSTOP, SIGTSTP,
    SIGTTIN, SIGTTOU, SIG_DFL, SIG_IGN,
};
use haisuli_rt::abi::USER_LIMIT;

/// SIGURG: urgent data on a socket (ignored by default).
const SIGURG: u64 = 23;
/// SIGWINCH: the terminal's size changed (ignored by default).
const SIGWINCH: u64 = 28;

/// What happens to a process that is sent a signal it does not ignore or catch.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Terminate the process; its wait status is the signal number.
    Term,
    /// Stop the process until it is continued.
    Stop,
    /// Continue the process if it is stopped.
    Cont,
    /// Do nothing.
    Ignore,
}

/// The default action of `sig`.
pub(crate) fn default_action(sig: u64) -> Action {
    match sig {
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => Action::Stop,
        SIGCONT => Action::Cont,
        SIGCHLD | SIGURG | SIGWINCH => Action::Ignore,
        _ => Action::Term,
    }
}

/// Whether `sig` is a signal number that can be sent (0 is the existence check of `kill`).
pub(crate) fn valid(sig: u64) -> bool {
    sig < NSIG
}

/// Whether a process may catch, ignore or block `sig`.
///
/// POSIX: SIGKILL and SIGSTOP can be neither caught, ignored nor blocked.
pub(crate) fn can_change_disposition(sig: u64) -> bool {
    sig != SIGKILL && sig != SIGSTOP && sig != 0 && valid(sig)
}

/// A set of signals, laid out like a Linux `sigset_t` (signal n is bit n - 1).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SigSet(u64);

impl SigSet {
    /// The empty set.
    pub(crate) const EMPTY: SigSet = SigSet(0);
    /// Every signal that exists.
    const ALL: u64 = (1 << (NSIG - 1)) - 1;

    /// The set with the given `sigset_t` bits (unknown signals dropped).
    pub(crate) fn from_bits(bits: u64) -> SigSet {
        SigSet(bits & Self::ALL)
    }

    /// The `sigset_t` bits of the set.
    pub(crate) fn bits(self) -> u64 {
        self.0
    }

    /// The set without SIGKILL and SIGSTOP, which cannot be blocked.
    pub(crate) fn blockable(self) -> SigSet {
        SigSet(self.0 & !(signal::bit(SIGKILL) | signal::bit(SIGSTOP)))
    }

    /// Adds `sig` to the set.
    pub(crate) fn add(&mut self, sig: u64) {
        self.0 |= signal::bit(sig);
    }

    /// Removes `sig` from the set.
    pub(crate) fn remove(&mut self, sig: u64) {
        self.0 &= !signal::bit(sig);
    }

    /// Whether `sig` is in the set.
    pub(crate) fn contains(self, sig: u64) -> bool {
        self.0 & signal::bit(sig) != 0
    }

    /// The signals in `self` but not in `other`.
    pub(crate) fn without(self, other: SigSet) -> SigSet {
        SigSet(self.0 & !other.0)
    }

    /// The signals in either set.
    pub(crate) fn union(self, other: SigSet) -> SigSet {
        SigSet(self.0 | other.0)
    }

    /// The signals in the set, lowest first.
    pub(crate) fn iter(self) -> impl Iterator<Item = u64> {
        (1..NSIG).filter(move |&sig| self.contains(sig))
    }

    /// Removes every signal whose default action is [`Action::Stop`].
    ///
    /// POSIX: SIGCONT discards pending stop signals.
    pub(crate) fn drop_stops(&mut self) {
        for sig in 1..NSIG {
            if default_action(sig) == Action::Stop {
                self.remove(sig);
            }
        }
    }
}

/// A signal's disposition, as `rt_sigaction` sets it (a Linux `struct sigaction`).
#[derive(Clone, Copy)]
pub(crate) struct SigAction {
    /// `SIG_DFL`, `SIG_IGN` or the address of the handler.
    pub(crate) handler: u64,
    /// `SA_*` flags.
    pub(crate) flags: u64,
    /// Where the handler returns to; it must hand the frame's mask back and
    /// make the `rt_sigreturn` call.
    pub(crate) restorer: u64,
    /// Signals blocked while the handler runs (besides the signal itself).
    pub(crate) mask: SigSet,
}

/// What a process does with a signal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// The [`default_action`].
    Default,
    /// Nothing: the signal is discarded.
    Ignore,
    /// Run a handler.
    Catch,
}

impl SigAction {
    /// The disposition every signal starts with.
    pub(crate) const DEFAULT: SigAction = SigAction {
        handler: SIG_DFL,
        flags: 0,
        restorer: 0,
        mask: SigSet::EMPTY,
    };

    /// Words in a `struct sigaction` (handler, flags, restorer, mask).
    pub(crate) const WORDS: usize = 4;

    /// Parses a `struct sigaction`.
    pub(crate) fn from_words(w: &[u64; Self::WORDS]) -> SigAction {
        SigAction {
            handler: w[0],
            flags: w[1],
            restorer: w[2],
            mask: SigSet::from_bits(w[3]),
        }
    }

    /// The action as a `struct sigaction`.
    pub(crate) fn to_words(self) -> [u64; Self::WORDS] {
        [self.handler, self.flags, self.restorer, self.mask.bits()]
    }

    /// Whether a process may install this action.
    ///
    /// A handler must be a user address and come with a restorer, which is the
    /// only way back from it (HaisuliOS has no vDSO to supply one). The kernel
    /// checks the addresses again when it enters the handler.
    pub(crate) fn is_valid(&self) -> bool {
        match self.handler {
            SIG_DFL | SIG_IGN => true,
            h => h < USER_LIMIT && self.flags & SA_RESTORER != 0 && self.restorer < USER_LIMIT,
        }
    }

    /// What this action does with `sig`.
    pub(crate) fn disposition(&self, sig: u64) -> Disposition {
        match self.handler {
            SIG_IGN => Disposition::Ignore,
            SIG_DFL if default_action(sig) == Action::Ignore => Disposition::Ignore,
            SIG_DFL => Disposition::Default,
            _ => Disposition::Catch,
        }
    }

    /// The signals blocked while the handler for `sig` runs.
    pub(crate) fn blocks_while_handling(&self, sig: u64) -> SigSet {
        let mut set = self.mask;
        if self.flags & (SA_NODEFER | SA_RESETHAND) == 0 {
            set.add(sig);
        }
        set.blockable()
    }
}

/// The signal state of a process.
#[derive(Clone, Copy)]
pub(crate) struct Signals {
    /// The disposition of each signal, by number (inherited across `fork`).
    pub(crate) actions: [SigAction; NSIG as usize],
    /// Signals that are not delivered for now (inherited across `fork`).
    pub(crate) blocked: SigSet,
    /// Signals sent to the process that have not acted on it yet.
    pub(crate) pending: SigSet,
    /// The pid that sent each pending signal (for `siginfo_t.si_pid`).
    pub(crate) senders: [u64; NSIG as usize],
}

impl Signals {
    /// The state of a process that has changed nothing.
    pub(crate) const DEFAULT: Signals = Signals {
        actions: [SigAction::DEFAULT; NSIG as usize],
        blocked: SigSet::EMPTY,
        pending: SigSet::EMPTY,
        senders: [0; NSIG as usize],
    };

    /// What a child inherits across `fork`: the dispositions and the mask, but
    /// not the pending signals.
    pub(crate) fn for_child(&self) -> Signals {
        Signals {
            actions: self.actions,
            blocked: self.blocked,
            ..Signals::DEFAULT
        }
    }
}
