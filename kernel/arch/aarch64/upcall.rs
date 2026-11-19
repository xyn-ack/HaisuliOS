// SPDX-License-Identifier: MIT

//! The upcall frame.
//!
//! The frame that makes a thread enter a handler ([`crate::upcall`]), in the Linux arm64 layout
//! (`struct rt_sigframe`: `siginfo`, then `ucontext`, whose `uc_mcontext` is a
//! `struct sigcontext` with the FP/SIMD registers in an `fpsimd_context` record of its
//! `__reserved` area): [`Frame`] is what the kernel pushes on the user stack on the way into a
//! handler, and [`SavedContext`] is what `rt_sigreturn` reads back. The handler returns through
//! the link register to its restorer with the stack pointer at the frame, where a libc ported
//! later finds `siginfo_t` and `ucontext_t`.

use super::entry::{TrapFrame, SPSR_FLAGS, USER_SPSR};
use crate::upcall::{is_user_address, Upcall};
use haisuli_abi::upcall::UC_SIGMASK;

/// `siginfo_t.si_code` of a signal sent by a process (Linux `SI_USER`).
const SI_USER: u32 = 0;

/// `struct siginfo`, at the start of the frame.
const SIGINFO: usize = 0;
/// `struct ucontext`, after the 128 bytes of `siginfo`.
const UC: usize = 128;
/// `ucontext.uc_sigmask`: after `uc_flags`, `uc_link` and `uc_stack`.
const SIGMASK: usize = UC + 40;
/// `ucontext.uc_mcontext` (a `struct sigcontext`, 16-byte aligned), after the 128 bytes that
/// Linux reserves for the signal mask.
const MCONTEXT: usize = UC + 176;
/// `sigcontext.__reserved`: after `fault_address`, `regs[31]`, `sp`, `pc` and `pstate`, 16-byte
/// aligned.
const RESERVED: usize = MCONTEXT + 288;
/// The size of `__reserved`.
const RESERVED_SIZE: usize = 4096;
const _: () = assert!(
    SIGMASK - SIGINFO == UC_SIGMASK,
    "the ABI's `UC_SIGMASK` is wrong"
);

/// The `fpsimd_context` record, first in `__reserved`: its header (`magic`, `size`), `fpsr`,
/// `fpcr`, then the 32 vector registers, 16 bytes each.
const FPSIMD: usize = RESERVED;
const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD_SIZE: u32 = 528;
const FPSIMD_FPSR: usize = FPSIMD + 8;
const FPSIMD_FPCR: usize = FPSIMD + 12;
const FPSIMD_VREGS: usize = FPSIMD + 16;
// After it, the end record (`magic` 0, `size` 0): the frame is zeroed.

/// Words of `struct sigcontext` that the kernel fills in, by offset from `uc_mcontext`.
#[derive(Clone, Copy)]
#[repr(usize)]
enum SigContext {
    /// `regs[0]`; `regs[n]` follows at `8 * n`.
    Regs = 8,
    Sp = 256,
    Pc = 264,
    Pstate = 272,
}

impl SigContext {
    /// The field's offset in the signal frame.
    const fn offset(self) -> usize {
        MCONTEXT + self as usize
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

/// Bytes in a frame: `siginfo` and `ucontext`, with all of `__reserved`.
const SIZE: usize = RESERVED + RESERVED_SIZE;

/// The signal frame the kernel writes below the user stack pointer to run a handler.
pub(crate) struct Frame {
    bytes: [u8; SIZE],
}

fn put(bytes: &mut [u8], at: usize, v: u64) {
    bytes[at..at + 8].copy_from_slice(&v.to_ne_bytes());
}

fn put32(bytes: &mut [u8], at: usize, v: u32) {
    bytes[at..at + 4].copy_from_slice(&v.to_ne_bytes());
}

fn get(bytes: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&bytes[at..at + 8]);
    u64::from_ne_bytes(w)
}

fn get32(bytes: &[u8], at: usize) -> u32 {
    let mut w = [0u8; 4];
    w.copy_from_slice(&bytes[at..at + 4]);
    u32::from_ne_bytes(w)
}

impl Frame {
    /// Builds the frame for running upcall `up` in a task interrupted at `regs`.
    pub(crate) fn new(up: &Upcall, regs: &TrapFrame) -> Frame {
        let mut f = Frame { bytes: [0; SIZE] };
        let b = &mut f.bytes;
        for (n, &x) in regs.x.iter().enumerate() {
            put(b, SigContext::Regs.offset() + 8 * n, x);
        }
        put(b, SigContext::Sp.offset(), regs.sp);
        put(b, SigContext::Pc.offset(), regs.elr);
        put(b, SigContext::Pstate.offset(), regs.spsr);
        put32(b, FPSIMD, FPSIMD_MAGIC);
        put32(b, FPSIMD + 4, FPSIMD_SIZE);
        put32(b, FPSIMD_FPSR, regs.fpsr as u32);
        put32(b, FPSIMD_FPCR, regs.fpcr as u32);
        for (n, &v) in regs.v.iter().enumerate() {
            put(b, FPSIMD_VREGS + 8 * n, v);
        }
        put(b, SIGMASK, up.mask);
        put(b, SigInfo::Signo.offset(), up.arg);
        put(b, SigInfo::Code.offset(), SI_USER as u64);
        put(b, SigInfo::Pid.offset(), up.sender);
        f
    }

    /// The frame as bytes, for copying to the user stack.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Where the frame goes for a task whose stack pointer is `sp`: below it, 16-byte aligned
    /// (arm64 has no red zone). `None` if `sp` is too low to hold it (`sp` is whatever user code
    /// left there).
    pub(crate) fn address(sp: u64) -> Option<u64> {
        Some(sp.checked_sub(SIZE as u64)? & !15)
    }

    /// Sets up `regs` to enter the handler of `up`, with the frame at `frame_addr`.
    pub(crate) fn enter_handler(regs: &mut TrapFrame, up: &Upcall, frame_addr: u64) {
        regs.elr = up.handler;
        regs.sp = frame_addr;
        regs.x[0] = up.arg;
        regs.x[1] = frame_addr + SIGINFO as u64;
        regs.x[2] = frame_addr + UC as u64;
        regs.x[29] = 0;
        regs.x[30] = up.restorer;
        regs.spsr = USER_SPSR | (regs.spsr & SPSR_FLAGS);
    }
}

/// The frame that `rt_sigreturn` reads back, from the stack pointer the restorer has: the whole
/// frame.
pub(crate) struct SavedContext {
    bytes: [u8; SIZE],
}

impl SavedContext {
    /// An empty buffer for reading a context from user memory.
    pub(crate) fn empty() -> SavedContext {
        SavedContext { bytes: [0; SIZE] }
    }

    /// The buffer to fill from user memory.
    pub(crate) fn as_mut_bytes(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Restores the interrupted registers into `regs`; `false` if the context would not be safe
    /// to resume.
    ///
    /// The handler may have changed the context (that is how a handler can redirect a task), so
    /// it is treated as untrusted: the thread returns to EL0 with interrupts enabled whatever
    /// `pstate` says (only the condition flags come from it), and `pc` and `sp` must be user
    /// addresses. The FP/SIMD registers come back if their record is intact. (The mask in the
    /// frame is not the kernel's business: the restorer has handed it to the process server.)
    pub(crate) fn restore(&self, regs: &mut TrapFrame) -> bool {
        let b = &self.bytes;
        let (sp, pc) = (
            get(b, SigContext::Sp.offset()),
            get(b, SigContext::Pc.offset()),
        );
        if !is_user_address(pc) || !is_user_address(sp) {
            return false;
        }
        for (n, x) in regs.x.iter_mut().enumerate() {
            *x = get(b, SigContext::Regs.offset() + 8 * n);
        }
        regs.sp = sp;
        regs.elr = pc;
        regs.spsr = USER_SPSR | (get(b, SigContext::Pstate.offset()) & SPSR_FLAGS);
        if get32(b, FPSIMD) == FPSIMD_MAGIC && get32(b, FPSIMD + 4) == FPSIMD_SIZE {
            regs.fpsr = get32(b, FPSIMD_FPSR) as u64;
            regs.fpcr = get32(b, FPSIMD_FPCR) as u64;
            for (n, v) in regs.v.iter_mut().enumerate() {
                *v = get(b, FPSIMD_VREGS + 8 * n);
            }
        }
        true
    }
}
