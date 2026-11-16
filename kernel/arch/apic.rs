// SPDX-License-Identifier: GPL-2.0

//! Local APIC.
//!
//! Each CPU's local APIC: its timer (the scheduler's tick), inter-processor interrupts, and the
//! INIT / start-up sequence that wakes the other CPUs.
//!
//! The registers are memory-mapped at [`APIC_BASE`], which the boot page tables map (uncached) in
//! the kernel half of every address space; every CPU sees its own APIC there. Access is limited
//! to the registers in [`Reg`], at their fixed offsets, which is what makes it safe. External
//! interrupts still come from the 8259 PICs: the boot CPU's APIC passes them through (LINT0 in
//! `ExtINT` mode, the PC's "virtual wire"), and the other CPUs mask them, so device IRQs always
//! reach the boot CPU.
//!
//! Time is measured with the PIT's counter 2, which needs no interrupt: [`wait_us`] busy-waits
//! on it while the kernel starts the other CPUs, and the APIC timer is calibrated against it.

use super::boot::APIC_BASE;
use super::cpu::{self, Device};
use super::x86::Msr;
use core::sync::atomic::{AtomicU32, Ordering};

/// The local APIC registers the kernel uses, by offset (SDM 11.4.1).
#[derive(Clone, Copy)]
#[repr(u64)]
enum Reg {
    /// The APIC id, in bits 24..32.
    Id = 0x20,
    /// Task priority: 0 accepts every interrupt.
    TaskPriority = 0x80,
    /// End of interrupt: write 0.
    EndOfInterrupt = 0xB0,
    /// Spurious interrupt vector, and the APIC's software enable bit.
    Spurious = 0xF0,
    /// Interrupt command register, low half: writing it sends the IPI.
    CommandLow = 0x300,
    /// Interrupt command register, high half: the destination APIC id in bits 24..32.
    CommandHigh = 0x310,
    /// The timer's local vector table entry.
    LvtTimer = 0x320,
    /// LINT0's local vector table entry (the PICs, on a PC).
    LvtLint0 = 0x350,
    /// LINT1's local vector table entry (NMI, on a PC).
    LvtLint1 = 0x360,
    /// The timer's initial count.
    TimerInitial = 0x380,
    /// The timer's current count.
    TimerCurrent = 0x390,
    /// The timer's divide configuration.
    TimerDivide = 0x3E0,
}

/// IA32_APIC_BASE bit 11: the APIC is enabled.
const APIC_GLOBAL_ENABLE: u64 = 1 << 11;
/// IA32_APIC_BASE bits 12 and up: where the registers are.
const APIC_BASE_MASK: u64 = !0xFFF;
/// The spurious-vector register's software enable bit.
const SOFTWARE_ENABLE: u32 = 1 << 8;
/// A local vector table entry's mask bit.
const LVT_MASKED: u32 = 1 << 16;
/// The timer's periodic mode (in [`Reg::LvtTimer`]).
const TIMER_PERIODIC: u32 = 1 << 17;
/// Delivery modes (in an LVT entry or [`Reg::CommandLow`]).
const DELIVERY_NMI: u32 = 0b100 << 8;
const DELIVERY_INIT: u32 = 0b101 << 8;
const DELIVERY_STARTUP: u32 = 0b110 << 8;
const DELIVERY_EXTINT: u32 = 0b111 << 8;
/// [`Reg::CommandLow`]: the IPI has not been delivered yet.
const DELIVERY_PENDING: u32 = 1 << 12;
/// [`Reg::CommandLow`]: level assert (required for INIT).
const LEVEL_ASSERT: u32 = 1 << 14;
/// [`Reg::TimerDivide`]: count at the bus clock divided by 16.
const DIVIDE_BY_16: u32 = 0b011;

/// The vector of the APIC timer: the scheduler's tick.
pub(crate) const TIMER_VECTOR: u8 = 0xF0;
/// The vector of the IPI that makes a CPU look at the scheduler again.
pub(crate) const RESCHEDULE_VECTOR: u8 = 0xF1;
/// The vector the APIC uses for spurious interrupts, which need no end of interrupt.
pub(crate) const SPURIOUS_VECTOR: u8 = 0xFF;

/// The scheduler's tick.
pub(crate) const TICK_MS: u32 = 10;
/// APIC timer counts per tick, measured by [`calibrate_timer`].
static TIMER_COUNT: AtomicU32 = AtomicU32::new(0);

fn read(reg: Reg) -> u32 {
    let addr = (APIC_BASE + reg as u64) as *const u32;
    // SAFETY: The boot page tables map the APIC's registers at `APIC_BASE` (uncached, in every
    // address space), and `init` checked that the APIC is there; `reg` is one of its 32-bit
    // registers. Reading them has no effect on memory.
    unsafe { core::ptr::read_volatile(addr) }
}

fn write(reg: Reg, v: u32) {
    let addr = (APIC_BASE + reg as u64) as *mut u32;
    // SAFETY: As in `read`. The APIC cannot access memory, so no value written to it can
    // affect memory safety.
    unsafe { core::ptr::write_volatile(addr, v) }
}

/// Enables this CPU's local APIC; the boot CPU's passes the PIC's interrupts through.
///
/// # Panics
///
/// Panics if the APIC's registers are not at [`APIC_BASE`], where the boot page tables map them.
pub(crate) fn init(boot_cpu: bool) {
    let base = cpu::rdmsr(Msr::ApicBase);
    assert!(
        base & APIC_BASE_MASK & 0xF_FFFF_FFFF == APIC_BASE,
        "local APIC at {:#x}, not at {:#x}",
        base & APIC_BASE_MASK,
        APIC_BASE
    );
    cpu::wrmsr(Msr::ApicBase, base | APIC_GLOBAL_ENABLE);
    write(Reg::Spurious, SOFTWARE_ENABLE | SPURIOUS_VECTOR as u32);
    write(Reg::TaskPriority, 0);
    if boot_cpu {
        write(Reg::LvtLint0, DELIVERY_EXTINT);
        write(Reg::LvtLint1, DELIVERY_NMI);
    } else {
        write(Reg::LvtLint0, LVT_MASKED);
        write(Reg::LvtLint1, LVT_MASKED);
    }
    write(Reg::LvtTimer, LVT_MASKED);
}

/// This CPU's APIC id.
pub(crate) fn id() -> u32 {
    read(Reg::Id) >> 24
}

/// Acknowledges the interrupt being handled (not for spurious ones).
pub(crate) fn end_of_interrupt() {
    write(Reg::EndOfInterrupt, 0);
}

/// Measures the APIC timer against the PIT: how many counts make a tick.
pub(crate) fn calibrate_timer() {
    write(Reg::TimerDivide, DIVIDE_BY_16);
    write(Reg::LvtTimer, LVT_MASKED);
    write(Reg::TimerInitial, u32::MAX);
    wait_us(TICK_MS * 1000);
    let elapsed = u32::MAX - read(Reg::TimerCurrent);
    write(Reg::TimerInitial, 0);
    TIMER_COUNT.store(elapsed.max(1000), Ordering::Relaxed);
}

/// Starts this CPU's periodic tick (interrupts must be enabled to see it).
pub(crate) fn start_timer() {
    write(Reg::TimerDivide, DIVIDE_BY_16);
    write(Reg::LvtTimer, TIMER_PERIODIC | TIMER_VECTOR as u32);
    write(Reg::TimerInitial, TIMER_COUNT.load(Ordering::Relaxed));
}

/// Sends an IPI with `command` (delivery mode and vector) to the CPU with APIC id `apic_id`.
fn send(apic_id: u32, command: u32) {
    while read(Reg::CommandLow) & DELIVERY_PENDING != 0 {
        core::hint::spin_loop();
    }
    write(Reg::CommandHigh, apic_id << 24);
    write(Reg::CommandLow, command);
    while read(Reg::CommandLow) & DELIVERY_PENDING != 0 {
        core::hint::spin_loop();
    }
}

/// Interrupts the CPU with APIC id `apic_id` with `vector`.
pub(crate) fn send_ipi(apic_id: u32, vector: u8) {
    send(apic_id, vector as u32);
}

/// Starts the CPU with APIC id `apic_id` at physical address `page` (below 1 MiB, page aligned)
/// in real mode: INIT, then the start-up IPI twice, as the MultiProcessor Specification says.
/// `started` tells whether it is running yet, so that the second IPI can be skipped.
pub(crate) fn start_cpu(apic_id: u32, page: u64, started: impl Fn() -> bool) {
    send(apic_id, DELIVERY_INIT | LEVEL_ASSERT);
    wait_us(10_000);
    for _ in 0..2 {
        send(apic_id, DELIVERY_STARTUP | (page >> 12) as u32);
        wait_us(200);
        if started() {
            return;
        }
    }
}

// ---- The PIT, as a stopwatch -----------------------------------------------------------------

/// The PIT's input clock, in Hz.
const PIT_HZ: u64 = 1_193_182;
/// PIT registers: counter 2's data, and the mode register.
const PIT_COUNTER2: u16 = 2;
const PIT_MODE: u16 = 3;
/// Mode register: counter 2, low byte then high byte, mode 0 (interrupt on terminal count).
const PIT_COUNTER2_ONE_SHOT: u8 = 0b1011_0000;
/// System control port B: counter 2's gate, the speaker, and counter 2's output.
const GATE2: u8 = 1 << 0;
const SPEAKER: u8 = 1 << 1;
const OUT2: u8 = 1 << 5;
/// The longest wait counter 2 can time in one go (16 bits at `PIT_HZ`).
const PIT_MAX_US: u32 = 50_000;

/// Busy-waits for about `us` microseconds.
pub(crate) fn wait_us(mut us: u32) {
    while us > 0 {
        let step = us.min(PIT_MAX_US);
        pit_wait(step);
        us -= step;
    }
}

fn pit_wait(us: u32) {
    let count = (PIT_HZ * us as u64 / 1_000_000).clamp(1, 0xFFFF) as u16;
    let port_b = Device::SystemControl.read8(0) & !(GATE2 | SPEAKER);
    Device::SystemControl.write8(0, port_b);
    Device::Pit.write8(PIT_MODE, PIT_COUNTER2_ONE_SHOT);
    let [low, high] = count.to_le_bytes();
    Device::Pit.write8(PIT_COUNTER2, low);
    Device::Pit.write8(PIT_COUNTER2, high);
    Device::SystemControl.write8(0, port_b | GATE2);
    while Device::SystemControl.read8(0) & OUT2 == 0 {
        core::hint::spin_loop();
    }
    Device::SystemControl.write8(0, port_b);
}
