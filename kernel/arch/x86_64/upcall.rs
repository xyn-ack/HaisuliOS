// SPDX-License-Identifier: MIT

//! The upcall frame.
//!
//! The frame that makes a thread enter a handler ([`crate::upcall`]), in the Linux x86_64 layout
//! (`struct rt_sigframe`): [`Frame`] is what the kernel pushes on the user stack on the way into
//! a handler, and [`SavedContext`] is what `rt_sigreturn` reads back. A libc ported later finds
//! `siginfo_t` and `ucontext_t` where it expects them.

use super::entry::{TrapFrame, USER_RFLAGS};
use super::x86::RFlag;
use crate::upcall::{is_user_address, Upcall};
use haisuli_abi::upcall::UC_SIGMASK;

/// `siginfo_t.si_code` of a signal sent by a process (Linux `SI_USER`).
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
const _: () = assert!(
    SIGMASK - UC == UC_SIGMASK,
    "the ABI's `UC_SIGMASK` is wrong"
);

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
    /// The upcall's mask, for old-style handlers.
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

    /// Builds the frame for running upcall `up` in a task interrupted at `regs`.
    pub(crate) fn new(up: &Upcall, regs: &TrapFrame) -> Frame {
        let mut f = Frame {
            bytes: [0; Frame::SIZE],
        };
        f.put(0, up.restorer); // `pretcode`: where the handler returns to.
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
            (C::OldMask, up.mask),
        ] {
            f.put(field.offset(), value);
        }
        f.put(SIGMASK, up.mask);
        f.put(SigInfo::Signo.offset(), up.arg);
        f.put(SigInfo::Code.offset(), SI_USER as u64);
        f.put(SigInfo::Pid.offset(), up.sender);
        f
    }

    /// The frame as bytes, for copying to the user stack.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Where the frame goes for a task whose stack pointer is `sp`.
    ///
    /// Below the 128-byte red zone, and aligned the way a `call` would leave
    /// the stack: the handler starts with `rsp + 8` a multiple of 16. `None`
    /// if `sp` is too low to hold it (`sp` is whatever user code left there).
    pub(crate) fn address(sp: u64) -> Option<u64> {
        let below = sp.checked_sub(128 + Frame::SIZE as u64)?;
        (below & !15).checked_sub(8)
    }

    /// Sets up `regs` to enter the handler of `up`, with the frame at `frame_addr`.
    pub(crate) fn enter_handler(regs: &mut TrapFrame, up: &Upcall, frame_addr: u64) {
        regs.rip = up.handler;
        regs.rsp = frame_addr;
        regs.rdi = up.arg;
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

    /// Restores the interrupted registers into `regs`; `false` if the context
    /// would not be safe to resume.
    ///
    /// The handler may have changed the context (that is how a handler can
    /// redirect a task), so it is treated as untrusted: the code and stack
    /// segments are always the user ones, only the arithmetic flags and DF
    /// come from it, and `rip` and `rsp` must be user addresses. (The mask in
    /// the frame is not the kernel's business: the restorer has handed it to
    /// the process server.)
    pub(crate) fn restore(&self, regs: &mut TrapFrame) -> bool {
        use SigContext as C;
        let get = |field: SigContext| self.word(field.offset());
        let (rsp, rip) = (get(C::Rsp), get(C::Rip));
        if !is_user_address(rip) || !is_user_address(rsp) {
            return false;
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
        true
    }
}
