// SPDX-License-Identifier: GPL-2.0

//! Per-CPU data.
//!
//! Each CPU has a small block of its own, which its GS base points to while it runs in the kernel.
//!
//! User mode has its own GS base (always 0: user code cannot change it, as `wrgsbase` is not
//! enabled and loading a GS selector gives base 0); every entry from user mode swaps the two with
//! `swapgs`, and every return swaps them back (see [`super::entry`]). So kernel code finds its
//! CPU's block at `gs:0` however it got into the kernel, and the entry stubs use it to find the
//! kernel stack before they have one.
//!
//! A thread may continue on another CPU after a context switch, so kernel code must ask
//! [`index`] again after anything that may switch; it must not keep the answer across a switch.

use super::cpu;
use super::x86::Msr;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// The most CPUs the kernel runs on; any more are left alone.
pub(crate) const MAX_CPUS: usize = 16;

/// One CPU's block. The entry stubs use the offsets below.
#[repr(C)]
struct PerCpu {
    /// Scratch: the user stack pointer while `syscall_entry` saves the user registers.
    user_rsp: AtomicU64,
    /// The top of the kernel stack of the thread running on this CPU, for `syscall_entry`.
    kernel_rsp: AtomicU64,
    /// The CPU's index: 0 for the boot CPU, then in the order the others started.
    index: AtomicU64,
}

/// Offset of [`PerCpu::user_rsp`], for `syscall_entry`.
pub(crate) const USER_RSP: usize = core::mem::offset_of!(PerCpu, user_rsp);
/// Offset of [`PerCpu::kernel_rsp`], for `syscall_entry`.
pub(crate) const KERNEL_RSP: usize = core::mem::offset_of!(PerCpu, kernel_rsp);
/// Offset of [`PerCpu::index`].
const INDEX: usize = core::mem::offset_of!(PerCpu, index);

static CPUS: [PerCpu; MAX_CPUS] = [const {
    PerCpu {
        user_rsp: AtomicU64::new(0),
        kernel_rsp: AtomicU64::new(0),
        index: AtomicU64::new(0),
    }
}; MAX_CPUS];

/// Makes this CPU CPU number `index`: points its GS base at its block.
///
/// Every CPU calls this first, before any code that asks [`index`] (the kernel's locks do).
///
/// # Panics
///
/// Panics if `index` is not below [`MAX_CPUS`].
pub(crate) fn init(index: usize) {
    let block = &CPUS[index];
    block.index.store(index as u64, Ordering::Relaxed);
    cpu::wrmsr(Msr::GsBase, block as *const PerCpu as u64);
    // The user GS base, which the first `swapgs` on the way to user mode installs.
    cpu::wrmsr(Msr::KernelGsBase, 0);
}

/// The index of the CPU this code runs on.
pub(crate) fn index() -> usize {
    let index: u64;
    // SAFETY: In the kernel, GS points at this CPU's `PerCpu` (see `init` and the module
    // documentation), whose `index` field is an `AtomicU64` at offset `INDEX`; this only reads
    // it.
    unsafe {
        asm!(
            "mov {}, gs:[{offset}]",
            out(reg) index,
            offset = const INDEX,
            options(nostack, preserves_flags, readonly),
        )
    };
    index as usize
}

/// Sets the kernel stack that a `syscall` on this CPU switches to.
pub(crate) fn set_syscall_stack(top: u64) {
    CPUS[index()].kernel_rsp.store(top, Ordering::Relaxed);
}
