// SPDX-License-Identifier: MIT

//! Per-CPU data.
//!
//! Each CPU has a small block of its own, which its TPIDR_EL1 points to. User mode cannot read
//! or write TPIDR_EL1, so, unlike x86's GS, it needs no swapping on the way in and out of the
//! kernel.
//!
//! A thread may continue on another CPU after a context switch, so kernel code must ask
//! [`index`] again after anything that may switch; it must not keep the answer across a switch.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// The most CPUs the kernel runs on; any more are left alone.
pub(crate) const MAX_CPUS: usize = 16;

/// What [`PerCpu::interrupt`] holds when no interrupt is being handled.
const NO_INTERRUPT: u64 = u64::MAX;

/// One CPU's block.
struct PerCpu {
    /// The CPU's index: 0 for the boot CPU, then in the order the others started.
    index: AtomicU64,
    /// The GIC interrupt id of the CPU's own interrupt being handled (the tick or a reschedule),
    /// whose end of interrupt is still due.
    interrupt: AtomicU64,
}

static CPUS: [PerCpu; MAX_CPUS] = [const {
    PerCpu {
        index: AtomicU64::new(0),
        interrupt: AtomicU64::new(NO_INTERRUPT),
    }
}; MAX_CPUS];

/// Makes this CPU CPU number `index`: points its TPIDR_EL1 at its block.
///
/// Every CPU calls this first, before any code that asks [`index`] (the kernel's locks do).
///
/// # Panics
///
/// Panics if `index` is not below [`MAX_CPUS`].
pub(crate) fn init(index: usize) {
    let block = &CPUS[index];
    block.index.store(index as u64, Ordering::Relaxed);
    // SAFETY: TPIDR_EL1 is the kernel's own register; it points at a `static` block for good.
    unsafe {
        asm!(
            "msr tpidr_el1, {}",
            in(reg) block as *const PerCpu as u64,
            options(nomem, nostack, preserves_flags),
        )
    };
}

/// This CPU's block (the boot CPU's before [`init`], so that an early panic can still report).
fn this_cpu() -> &'static PerCpu {
    let block: u64;
    // SAFETY: Reading TPIDR_EL1 has no side effects.
    unsafe { asm!("mrs {}, tpidr_el1", out(reg) block, options(nomem, nostack, preserves_flags)) };
    if block == 0 {
        return &CPUS[0];
    }
    // SAFETY: Only `init` writes TPIDR_EL1, with the address of an element of `CPUS`, which
    // lives forever.
    unsafe { &*(block as *const PerCpu) }
}

/// The index of the CPU this code runs on.
pub(crate) fn index() -> usize {
    this_cpu().index.load(Ordering::Relaxed) as usize
}

/// Records that this CPU handles its own interrupt `intid`, whose end of interrupt is due.
pub(crate) fn set_interrupt(intid: u64) {
    this_cpu().interrupt.store(intid, Ordering::Relaxed);
}

/// The interrupt that [`set_interrupt`] recorded (`None` if there is none), forgetting it.
pub(crate) fn take_interrupt() -> Option<u64> {
    match this_cpu().interrupt.swap(NO_INTERRUPT, Ordering::Relaxed) {
        NO_INTERRUPT => None,
        intid => Some(intid),
    }
}
