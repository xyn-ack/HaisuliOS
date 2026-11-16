// SPDX-License-Identifier: GPL-2.0

//! The GICv3 CPU interface.
//!
//! Each CPU's side of the GIC, through system registers: taking an interrupt and ending it, and
//! sending software-generated interrupts (SGIs) to other CPUs. The distributor and the
//! redistributors, memory-mapped, are the platform's business (which also enables the CPU's own
//! interrupts, [`TICK`] and [`RESCHEDULE`], in each CPU's redistributor).
//!
//! Interrupt ids: SGIs 0 to 15, each CPU's own; PPIs 16 to 31, each CPU's own devices (the timer);
//! SPIs from [`SPI_FIRST`] on, the machine's devices; [`SPECIAL`] and up are no interrupt.

use super::cpu::MPIDR_AFFINITY;
use super::percpu::MAX_CPUS;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// The interrupt id of the scheduler's tick: the virtual timer's PPI (see [`super::timer`]).
pub(crate) const TICK: u64 = 27;
/// The interrupt id of the reschedule IPI: SGI 0.
pub(crate) const RESCHEDULE: u64 = 0;
/// The first SPI, the machine's interrupt line 0.
pub(crate) const SPI_FIRST: u64 = 32;
/// Interrupt ids from here on are special: 1023 means that there was none to take.
pub(crate) const SPECIAL: u64 = 1020;

/// Each CPU's affinity (MPIDR_EL1), by CPU index, for sending it SGIs.
static AFFINITY: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// ICC_SRE_EL1: system register access to the CPU interface.
const SRE: u64 = 1 << 0;
/// ICC_PMR_EL1: the lowest priority that still interrupts (all of them).
const PRIORITY_MASK_ALL: u64 = 0xFF;

/// Enables this CPU's interface for group 1 interrupts of every priority, and records its
/// affinity.
pub(crate) fn init_cpu() {
    AFFINITY[super::percpu::index()].store(super::cpu::mpidr(), Ordering::Relaxed);
    // SAFETY: These registers only configure this CPU's interrupt interface; interrupts stay
    // masked (PSTATE.I) until the CPU idles or returns to user mode.
    unsafe {
        asm!(
            "mrs {tmp}, S3_0_C12_C12_5", // ICC_SRE_EL1
            "orr {tmp}, {tmp}, #{sre}",
            "msr S3_0_C12_C12_5, {tmp}",
            "isb",
            "msr S3_0_C4_C6_0, {pmr}",   // ICC_PMR_EL1
            "msr S3_0_C12_C12_3, xzr",   // ICC_BPR1_EL1: no subpriorities
            "mov {tmp}, #1",
            "msr S3_0_C12_C12_7, {tmp}", // ICC_IGRPEN1_EL1: group 1 enabled
            "isb",
            tmp = out(reg) _,
            sre = const SRE,
            pmr = in(reg) PRIORITY_MASK_ALL,
            options(nomem, nostack, preserves_flags),
        )
    };
}

/// Takes the highest-priority pending interrupt: its id (from [`SPECIAL`] on if there was none).
pub(crate) fn acknowledge() -> u64 {
    let iar: u64;
    // SAFETY: Reading ICC_IAR1_EL1 marks the interrupt active on this CPU, which
    // `end_of_interrupt` ends; it touches no memory.
    unsafe { asm!("mrs {}, S3_0_C12_C12_0", out(reg) iar, options(nomem, nostack)) };
    iar & 0xFF_FFFF
}

/// Ends interrupt `intid` on this CPU (nothing for `None`).
pub(crate) fn end_of_interrupt(intid: Option<u64>) {
    let Some(intid) = intid else {
        return;
    };
    // SAFETY: Writing ICC_EOIR1_EL1 ends the interrupt on this CPU's interface; it touches no
    // memory.
    unsafe { asm!("msr S3_0_C12_C12_1, {}", "isb", in(reg) intid, options(nomem, nostack)) };
}

/// Sends the reschedule SGI to CPU `cpu` (an index).
pub(crate) fn send_reschedule(cpu: usize) {
    let mpidr = AFFINITY[cpu].load(Ordering::Relaxed) & MPIDR_AFFINITY;
    let (aff0, aff1, aff2, aff3) = (
        mpidr & 0xFF,
        (mpidr >> 8) & 0xFF,
        (mpidr >> 16) & 0xFF,
        (mpidr >> 32) & 0xFF,
    );
    // ICC_SGI1R_EL1: the target list holds Aff0 modulo 16, RS selects which 16.
    let value = (1 << (aff0 % 16))
        | aff1 << 16
        | RESCHEDULE << 24
        | aff2 << 32
        | (aff0 / 16) << 44
        | aff3 << 48;
    // SAFETY: Writing ICC_SGI1R_EL1 sends an interrupt to another CPU; it touches no memory. The
    // `dsb` makes this CPU's earlier stores visible to the other one before it is interrupted.
    unsafe { asm!("dsb ish", "msr S3_0_C12_C11_5, {}", "isb", in(reg) value, options(nostack)) };
}
