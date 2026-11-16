// SPDX-License-Identifier: GPL-2.0

//! The PC platform.
//!
//! What [`crate::platform`] re-exports, for the PC (and QEMU's emulation of it): [`Pc`]
//! implements the contract's traits with Multiboot and the ACPI tables, the 8259 PICs, the COM1
//! UART, the 8254 timer and `isa-debug-exit`. Generic code never names the modules below.
//!
//! The PC is built on x86_64: its devices are reached with port I/O
//! ([`crate::arch::x86_64::cpu::Device`]).

pub(crate) mod acpi;
pub(crate) mod multiboot;
pub(crate) mod pic;
pub(crate) mod pit;
pub(crate) mod power;
pub(crate) mod uart;

use super::{BootInfo, CpuIds, InterruptController, Machine, Module};
use crate::arch::{Cpu, Processor};
use core::ops::Range;
use multiboot::MemoryType;

/// The PC.
pub(crate) struct Pc;

pub(crate) use Pc as Board;

/// COM1 for the log, the PIT for delays, the MADT's CPUs and QEMU's `isa-debug-exit`.
impl Machine for Pc {
    fn console_init() {
        uart::init();
    }

    fn console_write(byte: u8) {
        uart::write_byte(byte);
    }

    fn wait_us(us: u32) {
        pit::wait_us(us);
    }

    fn cpus() -> CpuIds {
        acpi::cpus()
    }

    /// Asks QEMU to end the emulation; on a real PC, the CPU halts.
    fn power_off(code: u32) -> ! {
        power::off(code);
        Cpu::halt()
    }
}

/// The Multiboot information: the memory map and the modules.
impl BootInfo for Pc {
    fn has_memory_map() -> bool {
        multiboot::has_memory_map()
    }

    fn memory() -> impl Iterator<Item = Range<u64>> {
        multiboot::memory_map()
            .filter(|&(_, _, kind)| kind == MemoryType::Available)
            .map(|(base, len, _)| base..base.saturating_add(len))
    }

    fn boot_data() -> impl Iterator<Item = Range<u64>> {
        multiboot::reserved_ranges().map(|(start, end)| start..end)
    }

    fn modules() -> impl Iterator<Item = Module> {
        multiboot::modules().map(|m| Module {
            data: m.data,
            name: m.name(),
        })
    }
}

/// The two cascaded 8259s.
impl InterruptController for Pc {
    const LINES: usize = pic::LINES;

    fn init() {
        pic::init();
    }

    fn is_reserved(line: usize) -> bool {
        line == pic::RESERVED_LINE
    }

    fn mask(line: usize) {
        pic::mask(line);
    }

    fn unmask(line: usize) {
        pic::unmask(line);
    }

    fn acknowledge(line: usize) -> bool {
        pic::acknowledge(line)
    }
}
