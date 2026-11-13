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
use crate::arch::x86::RFlag;
use crate::process::USER_RFLAGS;
use crate::vmm::USER_LIMIT;
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
    addr < USER_LIMIT
}

/// `siginfo_t.si_code` of a signal sent with `kill` (Linux `SI_USER`).
const SI_USER: u32 = 0;

// Offsets in the Linux x86_64 signal frame (`struct rt_sigframe`):
// `pretcode` (the restorer), then `struct ucontext`, then `struct siginfo`.
/// `struct ucontext` in the frame, after the 8-byte `pretcode`.
const UC: usize = 8;
/// `uc_flags`, `uc_link` and `uc_stack`: the part of `struct ucontext`
/// before `uc_mcontext`.
const UCONTEXT_HEADER: usize = 40;
/// `ucontext.uc_mcontext` (a `struct sigcontext`) in the frame.
const MCONTEXT: usize = UC + UCONTEXT_HEADER;
/// Words in a `struct sigcontext`, including its reserved ones.
const SIGCONTEXT_WORDS: usize = 32;
/// `ucontext.uc_sigmask` in the frame.
const SIGMASK: usize = MCONTEXT + SIGCONTEXT_WORDS * 8;
/// `struct siginfo` in the frame.
const SIGINFO: usize = SIGMASK + 8;

/// The words of Linux's x86_64 `struct sigcontext`, in order.
#[derive(Clone, Copy)]
#[repr(usize)]
enum SigContext {
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
    Rdi,
    Rsi,
    Rbp,
    Rbx,
    Rdx,
    Rax,
    Rcx,
    Rsp,
    Rip,
    Eflags,
    /// `cs`, `gs`, `fs` and `ss`, 16 bits each (see [`SS_SHIFT`]).
    Segments,
    /// The error code of the exception that raised the signal (0 here).
    Err,
    /// Its vector (0 here).
    TrapNo,
    /// The blocked signals, for old-style handlers.
    OldMask,
}

/// Where `ss` is in the [`SigContext::Segments`] word (`cs` is at bit 0).
const SS_SHIFT: u32 = 48;

impl SigContext {
    /// The field's offset in the signal frame.
    const fn offset(self) -> usize {
        MCONTEXT + self as usize * 8
    }
}

/// Offsets of the `struct siginfo` fields the kernel fills in.
#[derive(Clone, Copy)]
#[repr(usize)]
enum SigInfo {
    /// `si_signo` (i32), followed by `si_errno` (i32, 0 here).
    Signo = 0,
    /// `si_code` (i32).
    Code = 8,
    /// `si_pid` (i32) of the sender, followed by its `si_uid` (u32, 0 here).
    Pid = 16,
}

impl SigInfo {
    /// The field's offset in the signal frame.
    const fn offset(self) -> usize {
        SIGINFO + self as usize
    }
}
/// The RFLAGS bits a handler's `rt_sigreturn` may change: the arithmetic
/// flags and the direction flag. (Not TF, IF or the I/O privilege level.)
const USER_FLAGS: u64 = RFlag::bits(&[
    RFlag::Carry,
    RFlag::Parity,
    RFlag::Adjust,
    RFlag::Zero,
    RFlag::Sign,
    RFlag::Direction,
    RFlag::Overflow,
]);
/// RFLAGS bits that the ABI wants clear when a function (the handler) is called.
const CLEAR_ON_CALL: u64 = RFlag::bits(&[RFlag::Direction, RFlag::Trap]);

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
        f.put(0, act.restorer); // `pretcode`: where the handler returns to.
        use SigContext as C;
        let r = regs;
        for (field, value) in [
            (C::R8, r.r8),
            (C::R9, r.r9),
            (C::R10, r.r10),
            (C::R11, r.r11),
            (C::R12, r.r12),
            (C::R13, r.r13),
            (C::R14, r.r14),
            (C::R15, r.r15),
            (C::Rdi, r.rdi),
            (C::Rsi, r.rsi),
            (C::Rbp, r.rbp),
            (C::Rbx, r.rbx),
            (C::Rdx, r.rdx),
            (C::Rax, r.rax),
            (C::Rcx, r.rcx),
            (C::Rsp, r.rsp),
            (C::Rip, r.rip),
            (C::Eflags, r.rflags),
            (C::Segments, r.cs | r.ss << SS_SHIFT),
            // No exception raised the signal.
            (C::Err, 0),
            (C::TrapNo, 0),
            (C::OldMask, blocked.bits()),
        ] {
            f.put(field.offset(), value);
        }
        f.put(SIGMASK, blocked.bits());
        f.put(SigInfo::Signo.offset(), sig);
        f.put(SigInfo::Code.offset(), SI_USER as u64);
        f.put(SigInfo::Pid.offset(), sender);
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
        regs.rflags &= !CLEAR_ON_CALL;
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
        use SigContext as C;
        let get = |field: SigContext| self.word(field.offset());
        let (rsp, rip) = (get(C::Rsp), get(C::Rip));
        if !is_user_address(rip) || !is_user_address(rsp) {
            return None;
        }
        regs.r8 = get(C::R8);
        regs.r9 = get(C::R9);
        regs.r10 = get(C::R10);
        regs.r11 = get(C::R11);
        regs.r12 = get(C::R12);
        regs.r13 = get(C::R13);
        regs.r14 = get(C::R14);
        regs.r15 = get(C::R15);
        regs.rdi = get(C::Rdi);
        regs.rsi = get(C::Rsi);
        regs.rbp = get(C::Rbp);
        regs.rbx = get(C::Rbx);
        regs.rdx = get(C::Rdx);
        regs.rax = get(C::Rax);
        regs.rcx = get(C::Rcx);
        regs.rsp = rsp;
        regs.rip = rip;
        regs.rflags = USER_RFLAGS | (get(C::Eflags) & USER_FLAGS);
        Some(SigSet::from_bits(self.word(SIGMASK)).blockable())
    }
}
