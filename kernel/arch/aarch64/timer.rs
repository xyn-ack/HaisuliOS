// SPDX-License-Identifier: MIT

//! The generic timer.
//!
//! Each CPU's virtual timer is its tick (interrupt [`super::gic::TICK`]); the virtual counter,
//! which every CPU sees the same, measures short waits. The timer tells its frequency
//! (CNTFRQ_EL0), so nothing needs measuring.

use core::arch::asm;

/// The scheduler's tick.
pub(crate) const TICK_MS: u64 = 10;

/// CNTV_CTL_EL0: the timer is enabled (and its interrupt not masked).
const ENABLE: u64 = 1 << 0;

/// Counter ticks per second.
fn frequency() -> u64 {
    let v: u64;
    // SAFETY: Reading CNTFRQ_EL0 has no side effects.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// The virtual counter.
fn count() -> u64 {
    let v: u64;
    // SAFETY: Reading CNTVCT_EL0 has no side effects; the `isb` keeps the read in program order.
    unsafe {
        asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack, preserves_flags))
    };
    v
}

/// Busy-waits for about `us` microseconds.
pub(crate) fn wait_us(us: u32) {
    let end = count() + frequency() * us as u64 / 1_000_000;
    while count() < end {
        core::hint::spin_loop();
    }
}

/// Arms the timer to fire one tick from now.
pub(crate) fn rearm() {
    let ticks = frequency() * TICK_MS / 1000;
    // SAFETY: Writing CNTV_TVAL_EL0 only sets this CPU's timer.
    unsafe { asm!("msr cntv_tval_el0, {}", "isb", in(reg) ticks, options(nomem, nostack)) };
}

/// Starts this CPU's periodic tick (interrupts must be enabled to see it): every interrupt arms
/// the timer again ([`rearm`]).
pub(crate) fn start() {
    rearm();
    // SAFETY: Writing CNTV_CTL_EL0 only enables this CPU's timer.
    unsafe { asm!("msr cntv_ctl_el0, {}", "isb", in(reg) ENABLE, options(nomem, nostack)) };
}

/// Stops this CPU's timer (until [`start`]).
pub(crate) fn stop() {
    // SAFETY: As in `start`.
    unsafe { asm!("msr cntv_ctl_el0, xzr", "isb", options(nomem, nostack)) };
}
