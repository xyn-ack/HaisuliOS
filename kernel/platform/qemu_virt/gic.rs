// SPDX-License-Identifier: MIT

//! The interrupt controller.
//!
//! The machine's GICv3: its distributor, for the interrupt lines of the machine's devices (SPIs,
//! which [`crate::irq`] forwards to user-space drivers), and each CPU's redistributor, which
//! enables the CPU's own interrupts: the tick and the reschedule IPI (see
//! [`crate::arch::aarch64::gic`], the CPU interface, which takes and ends interrupts).
//!
//! Line `n` is SPI `n`, interrupt id 32 + `n`. Device lines go to the boot CPU only, as on the
//! PC. The registers are reached through the kernel's linear map (device memory, uncached), only
//! the GIC's own, at their fixed offsets; the GIC accesses no memory of the kernel (its ITS, which
//! would, is not used).

use crate::arch::aarch64::gic::{RESCHEDULE, SPI_FIRST, TICK};
use crate::arch::aarch64::{cpu, phys};
use core::sync::atomic::{AtomicU64, Ordering};

/// The interrupt lines: the first SPIs (QEMU's `virt` has its devices there).
pub(crate) const LINES: usize = 64;

/// Physical addresses of the distributor and of the redistributors (one 128 KiB frame per CPU),
/// and the size of the redistributors' region, from the device tree.
static DISTRIBUTOR: AtomicU64 = AtomicU64::new(0);
static REDISTRIBUTORS: AtomicU64 = AtomicU64::new(0);
static REDISTRIBUTORS_SIZE: AtomicU64 = AtomicU64::new(0);

/// Distributor registers (GICD_*), by offset.
mod gicd {
    pub(super) const CTLR: u64 = 0x0000;
    pub(super) const IGROUPR: u64 = 0x0080;
    pub(super) const ISENABLER: u64 = 0x0100;
    pub(super) const ICENABLER: u64 = 0x0180;
    pub(super) const IPRIORITYR: u64 = 0x0400;
    pub(super) const ICFGR: u64 = 0x0C00;
    pub(super) const IROUTER: u64 = 0x6000;
    /// CTLR: a register write is still in progress.
    pub(super) const CTLR_RWP: u32 = 1 << 31;
    /// CTLR (one security state): group 0 and group 1 enabled, affinity routing.
    pub(super) const CTLR_ENABLE: u32 = 1 << 0 | 1 << 1 | 1 << 4;
}

/// Redistributor registers (GICR_*), by offset from a CPU's frame.
mod gicr {
    pub(super) const TYPER: u64 = 0x0008;
    pub(super) const WAKER: u64 = 0x0014;
    /// The frame of the SGIs and PPIs, after the control frame.
    pub(super) const SGI: u64 = 0x1_0000;
    pub(super) const IGROUPR0: u64 = SGI + 0x0080;
    pub(super) const ISENABLER0: u64 = SGI + 0x0100;
    pub(super) const IPRIORITYR: u64 = SGI + 0x0400;
    /// One CPU's frames.
    pub(super) const STRIDE: u64 = 0x2_0000;
    /// TYPER: the last redistributor.
    pub(super) const TYPER_LAST: u64 = 1 << 4;
    /// WAKER: the CPU's interface is asleep (to be cleared), and its effect (to wait for).
    pub(super) const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
    pub(super) const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
}

/// The priority of every interrupt (the CPU interface lets everything below 0xFF through).
const PRIORITY: u32 = 0xA0;

/// Records where the GIC is (before [`init`]).
pub(crate) fn set_base(distributor: u64, redistributors: u64, redistributors_size: u64) {
    DISTRIBUTOR.store(distributor, Ordering::Relaxed);
    REDISTRIBUTORS.store(redistributors, Ordering::Relaxed);
    REDISTRIBUTORS_SIZE.store(redistributors_size, Ordering::Relaxed);
}

fn read32(address: u64) -> u32 {
    // SAFETY: `address` is one of the GIC's 32-bit registers in the linear map (see the module
    // documentation); reading it has no effect on memory.
    unsafe { core::ptr::read_volatile(phys::virt(address) as *const u32) }
}

fn write32(address: u64, value: u32) {
    // SAFETY: As in `read32`; the GIC does not access memory.
    unsafe { core::ptr::write_volatile(phys::virt(address) as *mut u32, value) }
}

fn read64(address: u64) -> u64 {
    // SAFETY: As in `read32`, for a 64-bit register.
    unsafe { core::ptr::read_volatile(phys::virt(address) as *const u64) }
}

fn write64(address: u64, value: u64) {
    // SAFETY: As in `write32`, for a 64-bit register.
    unsafe { core::ptr::write_volatile(phys::virt(address) as *mut u64, value) }
}

fn distributor(reg: u64) -> u64 {
    DISTRIBUTOR.load(Ordering::Relaxed) + reg
}

/// Waits until the distributor has applied the last change of its control or enable registers.
fn wait_for_distributor() {
    while read32(distributor(gicd::CTLR)) & gicd::CTLR_RWP != 0 {
        core::hint::spin_loop();
    }
}

/// The register holding interrupt `intid`'s bit, among the ones from `base` on (32 per register).
fn bit_register(base: u64, intid: u64) -> (u64, u32) {
    (base + intid / 32 * 4, 1 << (intid % 32))
}

/// Sets the distributor up: every line in group 1, level-triggered, routed to the boot CPU (this
/// one) and masked.
pub(crate) fn init() {
    write32(distributor(gicd::CTLR), 0);
    wait_for_distributor();
    let boot_cpu = cpu::mpidr();
    let route = (boot_cpu & 0xFF_0000_0000) | (boot_cpu & 0xFF_FFFF);
    let spis = SPI_FIRST..SPI_FIRST + LINES as u64;
    for intid in spis.clone().step_by(32) {
        let (reg, _) = bit_register(gicd::IGROUPR, intid);
        write32(distributor(reg), u32::MAX);
        let (reg, _) = bit_register(gicd::ICENABLER, intid);
        write32(distributor(reg), u32::MAX);
    }
    for intid in spis.clone().step_by(16) {
        write32(distributor(gicd::ICFGR + intid / 16 * 4), 0); // Level-triggered.
    }
    for intid in spis.clone().step_by(4) {
        write32(
            distributor(gicd::IPRIORITYR + intid),
            PRIORITY * 0x0101_0101,
        );
    }
    for intid in spis {
        write64(distributor(gicd::IROUTER + intid * 8), route);
    }
    wait_for_distributor();
    write32(distributor(gicd::CTLR), gicd::CTLR_ENABLE);
    wait_for_distributor();
}

/// The frames of this CPU's redistributor, found by its affinity.
fn this_redistributor() -> Option<u64> {
    let mpidr = cpu::mpidr();
    // GICR_TYPER's affinity: Aff3.Aff2.Aff1.Aff0 in bits 32..64.
    let affinity = ((mpidr >> 32) & 0xFF) << 24 | (mpidr & 0xFF_FFFF);
    let (base, size) = (
        REDISTRIBUTORS.load(Ordering::Relaxed),
        REDISTRIBUTORS_SIZE.load(Ordering::Relaxed),
    );
    (base..base + size)
        .step_by(gicr::STRIDE as usize)
        .map(|frame| (frame, read64(frame + gicr::TYPER)))
        .scan(false, |last, (frame, typer)| {
            // Up to and including the one marked last.
            (!core::mem::replace(last, typer & gicr::TYPER_LAST != 0)).then_some((frame, typer))
        })
        .find(|&(_, typer)| typer >> 32 == affinity)
        .map(|(frame, _)| frame)
}

/// Wakes this CPU's redistributor and enables the CPU's own interrupts there: the tick and the
/// reschedule IPI, in group 1.
///
/// # Panics
///
/// Panics if the CPU has no redistributor.
pub(crate) fn init_cpu() {
    let frame = this_redistributor().expect("no GIC redistributor for this CPU");
    let waker = read32(frame + gicr::WAKER);
    write32(frame + gicr::WAKER, waker & !gicr::WAKER_PROCESSOR_SLEEP);
    while read32(frame + gicr::WAKER) & gicr::WAKER_CHILDREN_ASLEEP != 0 {
        core::hint::spin_loop();
    }
    write32(frame + gicr::IGROUPR0, u32::MAX);
    for intid in [TICK, RESCHEDULE] {
        let reg = frame + gicr::IPRIORITYR + intid / 4 * 4;
        let shift = intid % 4 * 8;
        write32(reg, (read32(reg) & !(0xFF << shift)) | PRIORITY << shift);
    }
    write32(frame + gicr::ISENABLER0, 1 << TICK | 1 << RESCHEDULE);
}

/// Keeps `line` quiet until [`unmask`].
pub(crate) fn mask(line: usize) {
    let (reg, bit) = bit_register(gicd::ICENABLER, SPI_FIRST + line as u64);
    write32(distributor(reg), bit);
    wait_for_distributor();
}

/// Lets `line` interrupt again.
pub(crate) fn unmask(line: usize) {
    let (reg, bit) = bit_register(gicd::ISENABLER, SPI_FIRST + line as u64);
    write32(distributor(reg), bit);
}

/// Takes the interrupt on `line`: masks the line (its device keeps it asserted until its driver
/// has serviced it) and ends the interrupt.
pub(crate) fn acknowledge(line: usize) -> bool {
    mask(line);
    crate::arch::aarch64::gic::end_of_interrupt(Some(SPI_FIRST + line as u64));
    true
}
