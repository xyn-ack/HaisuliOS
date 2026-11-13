// SPDX-License-Identifier: GPL-2.0

//! Signals.
//!
//! Signal numbers, dispositions, default actions, signal sets and the signal frame.
//!
//! [`crate::process`] decides *when* a signal acts; this module holds what that decision needs:
//! each process's [`SigAction`]s, [`SigSet`]s for the pending and blocked signals, and the
//! [`Frame`] that the kernel pushes on the user stack to run a handler. The frame has the Linux
//! x86_64 layout (`struct rt_sigframe`), so code written for Linux signal handlers, such as a
//! future libc port, finds `siginfo_t` and `ucontext_t` where it expects them.

use crate::arch::entry::TrapFrame;
use haisuli_abi::signal::{
    self, NSIG, SA_NODEFER, SA_RESETHAND, SA_RESTORER, SIGCHLD, SIGCONT, SIGKILL, SIGSTOP, SIGTSTP,
    SIGTTIN, SIGTTOU, SIG_DFL, SIG_IGN,
};

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
        // SIGCHLD, SIGURG (23) and SIGWINCH (28).
        SIGCHLD | 23 | 28 => Action::Ignore,
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

    /// The lowest-numbered signal in the set.
    pub(crate) fn lowest(self) -> Option<u64> {
        (self.0 != 0).then(|| self.0.trailing_zeros() as u64 + 1)
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
    /// Where the handler returns to; it must make the `rt_sigreturn` call.
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

    /// Size of a Linux `struct sigaction` (handler, flags, restorer, mask).
    pub(crate) const SIZE: usize = 32;

    /// Parses a `struct sigaction` from user memory.
    pub(crate) fn from_bytes(b: &[u8; Self::SIZE]) -> SigAction {
        let word = |i: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&b[i * 8..i * 8 + 8]);
            u64::from_ne_bytes(w)
        };
        SigAction {
            handler: word(0),
            flags: word(1),
            restorer: word(2),
            mask: SigSet::from_bits(word(3)),
        }
    }

    /// Serialises the action as a `struct sigaction`.
    pub(crate) fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        let words = [self.handler, self.flags, self.restorer, self.mask.bits()];
        for (i, w) in words.iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&w.to_ne_bytes());
        }
        b
    }

    /// Whether a process may install this action.
    ///
    /// A handler must be a user address and come with a restorer, which is the
    /// only way back from it (HaisuliOS has no vDSO to supply one).
    pub(crate) fn is_valid(&self) -> bool {
        match self.handler {
            SIG_DFL | SIG_IGN => true,
            h => {
                is_user_address(h)
                    && self.flags & SA_RESTORER != 0
                    && is_user_address(self.restorer)
            }
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

/// Whether `addr` is in the user half of the address space.
///
/// Anything else must never reach `rip` or `rsp` on the way to user mode: a
/// non-canonical address would make `iretq` fault in the kernel.
pub(crate) fn is_user_address(addr: u64) -> bool {
    addr < 0x0000_8000_0000_0000
}

/// `siginfo_t.si_code` of a signal sent with `kill` (Linux `SI_USER`).
const SI_USER: u32 = 0;

// Offsets in the Linux x86_64 signal frame (`struct rt_sigframe`):
// `pretcode` (the restorer), then `struct ucontext`, then `struct siginfo`.
const UC: usize = 8;
/// `ucontext.uc_mcontext` (a `struct sigcontext`) in the frame.
const MCONTEXT: usize = UC + 40;
/// `ucontext.uc_sigmask` in the frame.
const SIGMASK: usize = MCONTEXT + 32 * 8;
/// `struct siginfo` in the frame.
const SIGINFO: usize = SIGMASK + 8;
/// `struct sigcontext`: r8 ... r15, rdi, rsi, rbp, rbx, rdx, rax, rcx, rsp, rip, eflags.
const MCONTEXT_WORDS: usize = 18;
/// The RFLAGS bits a handler's `rt_sigreturn` may change: CF PF AF ZF SF DF OF.
const USER_FLAGS: u64 = 0xCD5;
/// RFLAGS for user mode: IF, and the reserved bit 1.
const BASE_FLAGS: u64 = 0x202;
/// RFLAGS.DF and RFLAGS.TF, which the ABI wants clear when a function is called.
const DF_TF: u64 = 0x500;

/// The signal frame the kernel writes below the user stack pointer to run a handler.
pub(crate) struct Frame {
    bytes: [u8; Frame::SIZE],
}

impl Frame {
    /// Bytes in a frame: `pretcode`, `struct ucontext` and `struct siginfo`.
    pub(crate) const SIZE: usize = SIGINFO + 128;

    /// Builds the frame for running the handler of `sig`, sent by `sender`,
    /// in a process interrupted at `regs` with `blocked` signals blocked.
    pub(crate) fn new(
        sig: u64,
        sender: u64,
        act: &SigAction,
        regs: &TrapFrame,
        blocked: SigSet,
    ) -> Frame {
        let mut f = Frame {
            bytes: [0; Frame::SIZE],
        };
        f.put(0, act.restorer);
        let r = regs;
        let mcontext = [
            r.r8, r.r9, r.r10, r.r11, r.r12, r.r13, r.r14, r.r15, r.rdi, r.rsi, r.rbp, r.rbx,
            r.rdx, r.rax, r.rcx, r.rsp, r.rip, r.rflags,
        ];
        for (i, &w) in mcontext.iter().enumerate() {
            f.put(MCONTEXT + i * 8, w);
        }
        // cs, gs, fs and ss, as four 16-bit fields.
        f.put(MCONTEXT + 18 * 8, r.cs | (r.ss << 48));
        f.put(MCONTEXT + 21 * 8, blocked.bits()); // `oldmask`.
        f.put(SIGMASK, blocked.bits());
        // siginfo: si_signo, si_errno, si_code, then si_pid and si_uid.
        f.put(SIGINFO, sig);
        f.put(SIGINFO + 8, SI_USER as u64);
        f.put(SIGINFO + 16, sender);
        f
    }

    /// The frame as bytes, for copying to the user stack.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Where the frame goes for a process whose stack pointer is `sp`.
    ///
    /// Below the 128-byte red zone, and aligned the way a `call` would leave
    /// the stack: the handler starts with `rsp + 8` a multiple of 16. `None`
    /// if `sp` is too low to hold it (`sp` is whatever user code left there).
    pub(crate) fn address(sp: u64) -> Option<u64> {
        let below = sp.checked_sub(128 + Frame::SIZE as u64)?;
        (below & !15).checked_sub(8)
    }

    /// Sets up `regs` to enter the handler, with the frame at `frame_addr`.
    pub(crate) fn enter_handler(regs: &mut TrapFrame, sig: u64, act: &SigAction, frame_addr: u64) {
        regs.rip = act.handler;
        regs.rsp = frame_addr;
        regs.rdi = sig;
        regs.rsi = frame_addr + SIGINFO as u64;
        regs.rdx = frame_addr + UC as u64;
        regs.rax = 0;
        regs.rflags &= !DF_TF;
    }

    fn put(&mut self, at: usize, v: u64) {
        self.bytes[at..at + 8].copy_from_slice(&v.to_ne_bytes());
    }
}

/// The `ucontext` that `rt_sigreturn` reads back, from the frame's `uc` onwards.
pub(crate) struct SavedContext {
    bytes: [u8; SIGINFO - UC],
}

impl SavedContext {
    /// An empty buffer for reading a context from user memory.
    pub(crate) fn empty() -> SavedContext {
        SavedContext {
            bytes: [0; SIGINFO - UC],
        }
    }

    /// The buffer to fill from user memory.
    pub(crate) fn as_mut_bytes(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    fn word(&self, frame_offset: usize) -> u64 {
        let at = frame_offset - UC;
        let mut w = [0u8; 8];
        w.copy_from_slice(&self.bytes[at..at + 8]);
        u64::from_ne_bytes(w)
    }

    /// Restores the interrupted registers into `regs`, and returns the signal
    /// mask to go back to; `None` if the context would not be safe to resume.
    ///
    /// The handler may have changed the context (that is how a handler can
    /// redirect a process), so it is treated as untrusted: the code and stack
    /// segments are always the user ones, only the arithmetic flags and DF
    /// come from it, and `rip` and `rsp` must be user addresses.
    pub(crate) fn restore(&self, regs: &mut TrapFrame) -> Option<SigSet> {
        let w: [u64; MCONTEXT_WORDS] = core::array::from_fn(|i| self.word(MCONTEXT + i * 8));
        let (rsp, rip) = (w[15], w[16]);
        if !is_user_address(rip) || !is_user_address(rsp) {
            return None;
        }
        regs.r8 = w[0];
        regs.r9 = w[1];
        regs.r10 = w[2];
        regs.r11 = w[3];
        regs.r12 = w[4];
        regs.r13 = w[5];
        regs.r14 = w[6];
        regs.r15 = w[7];
        regs.rdi = w[8];
        regs.rsi = w[9];
        regs.rbp = w[10];
        regs.rbx = w[11];
        regs.rdx = w[12];
        regs.rax = w[13];
        regs.rcx = w[14];
        regs.rsp = rsp;
        regs.rip = rip;
        regs.rflags = BASE_FLAGS | (w[17] & USER_FLAGS);
        Some(SigSet::from_bits(self.word(SIGMASK)).blockable())
    }
}
