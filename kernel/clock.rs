// SPDX-License-Identifier: MIT

//! The monotonic clock.
//!
//! The time since boot, counted in the boot CPU's ticks ([`CpuInterrupts::TICK_MS`] apart): coarse,
//! but it never goes back, and it needs nothing of the hardware beyond the tick. It starts when
//! the boot CPU first idles, with the system about to run its first task.

use crate::arch::{percpu, Cpu, CpuInterrupts};
use crate::{task, vmm};
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::errno::{EFAULT, EINVAL};
use haisuli_abi::time::{CLOCK_MONOTONIC, NSEC_PER_SEC, TIMESPEC_LEN};

/// Ticks of the boot CPU since it started them.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Counts a tick; called on every CPU's tick, and counted on the boot CPU's.
pub(crate) fn tick() {
    if percpu::index() == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Nanoseconds since boot (in steps of a tick).
pub(crate) fn now_ns() -> u64 {
    TICKS.load(Ordering::Relaxed) * Cpu::TICK_MS * 1_000_000
}

/// Stores the time of `clock` at user address `out`, as a `struct timespec` (the
/// `clock_gettime` system call); only [`CLOCK_MONOTONIC`].
pub(crate) fn clock_gettime(clock: u64, out: u64) -> i64 {
    if clock != CLOCK_MONOTONIC {
        return -EINVAL;
    }
    let ns = now_ns();
    let mut timespec = [0u8; TIMESPEC_LEN];
    timespec[..8].copy_from_slice(&(ns / NSEC_PER_SEC).to_ne_bytes());
    timespec[8..].copy_from_slice(&(ns % NSEC_PER_SEC).to_ne_bytes());
    if vmm::copy_to_user(task::current_space(), out, &timespec) {
        0
    } else {
        -EFAULT
    }
}
