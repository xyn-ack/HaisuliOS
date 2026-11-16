// SPDX-License-Identifier: GPL-2.0

//! Hardware layer: the platform.
//!
//! The machine around the CPU: how the kernel is booted and what the loader left, the interrupt
//! controller, the serial line, the timer that measures boot-time delays, how the other CPUs
//! are found, and how the machine powers off. One implementation per platform; there is one per
//! architecture for now, so the architecture selects it (a feature could choose among boards
//! later).
//!
//! This module is the boundary: the rest of the kernel uses only what is re-exported here,
//! [`Board`], through the traits that make the compiler check the contract:
//!
//! - [`Machine`]: the early console (the kernel's log), start-up delays, the CPUs the firmware
//!   lists, and powering off;
//! - [`BootInfo`]: what the boot loader left, in platform-neutral terms: the memory, the memory
//!   its own data takes, and the boot modules;
//! - [`InterruptController`]: the platform's interrupt lines, which [`crate::irq`] forwards to
//!   user-space drivers.
//!
//! A platform may use the architecture it is built on, by name (its devices are reached with the
//! CPU's instructions); the architecture does not depend on the platform, except that the boot
//! entry speaks the boot loader's protocol (see [`crate::arch`]). Generic code never names a
//! platform's directory.
//!
//! Everything here may use unsafe Rust (see `main.rs`); the module documentation of each part
//! says what makes its interface safe.

#[cfg(target_arch = "x86_64")]
pub(crate) mod pc;
#[cfg(target_arch = "x86_64")]
use pc as imp;

use crate::arch::percpu::MAX_CPUS;
use core::ops::Range;

pub(crate) use imp::Board;

/// The machine: what the kernel needs from it to boot, log and stop.
pub(crate) trait Machine {
    /// Sets up the early console, the line that the kernel's log goes out on, for output.
    fn console_init();

    /// Sends `byte` to the early console, waiting until it takes it.
    fn console_write(byte: u8);

    /// Busy-waits for about `us` microseconds, without interrupts: the delays of booting (the
    /// architecture's [`start_cpus`](crate::arch::Processor::start_cpus), say).
    fn wait_us(us: u32);

    /// The hardware ids of the machine's enabled CPUs, as its firmware lists them (at most
    /// [`MAX_CPUS`]; none if it lists none: then the kernel runs on the boot CPU alone).
    fn cpus() -> CpuIds;

    /// Powers the machine off, with `code` as its exit status where it has one (an emulator's),
    /// or failing that, stops this CPU.
    fn power_off(code: u32) -> !;
}

/// What the boot loader left for the kernel.
///
/// Everything that [`boot_data`](Self::boot_data) lists stays as the loader left it for the
/// kernel's lifetime: the kernel never gets it for anything else.
pub(crate) trait BootInfo {
    /// Whether the loader described the memory (otherwise the kernel cannot run).
    fn has_memory_map() -> bool;

    /// The RAM that the kernel may use, by physical address range: everything the loader calls
    /// available (including what the kernel image and [`boot_data`](Self::boot_data) take).
    fn memory() -> impl Iterator<Item = Range<u64>>;

    /// The physical memory that holds the loader's data that the kernel still uses: the boot
    /// modules, and whatever describes them.
    fn boot_data() -> impl Iterator<Item = Range<u64>>;

    /// The boot modules, in the order the loader lists them.
    fn modules() -> impl Iterator<Item = Module>;
}

/// The platform's interrupt controller: device interrupt lines, numbered from 0.
///
/// When a line fires, the architecture's interrupt entry reports it as
/// [`Interrupt::Line`](crate::arch::Interrupt::Line).
pub(crate) trait InterruptController {
    /// The number of interrupt lines.
    const LINES: usize;

    /// Sets the controller up, with every device line masked.
    fn init();

    /// Whether `line` (below [`LINES`](Self::LINES)) can never be a device's.
    fn is_reserved(line: usize) -> bool;

    /// Keeps `line` quiet until [`unmask`](Self::unmask).
    fn mask(line: usize);

    /// Lets `line` interrupt again.
    fn unmask(line: usize);

    /// Takes the interrupt that arrived on `line`: masks the line (until its driver has serviced
    /// the device) and acknowledges the controller. Returns `false` if the interrupt was
    /// spurious (the line has nothing to deliver).
    fn acknowledge(line: usize) -> bool;
}

// The platform's type must implement the contract.
const _: () = {
    const fn board<T: Machine + BootInfo + InterruptController>() {}
    board::<Board>();
};

/// A boot module: a file that the boot loader loaded next to the kernel (GRUB's `module`, QEMU's
/// `-initrd`).
#[derive(Clone, Copy)]
pub(crate) struct Module {
    /// The file's contents.
    pub(crate) data: &'static [u8],
    /// A short name: the file name that the loader gives.
    pub(crate) name: &'static [u8],
}

/// The hardware ids of a machine's CPUs (APIC ids on a PC, MPIDR values on arm64).
pub(crate) struct CpuIds {
    ids: [u64; MAX_CPUS],
    len: usize,
}

impl CpuIds {
    /// No CPUs.
    pub(crate) const fn new() -> CpuIds {
        CpuIds {
            ids: [0; MAX_CPUS],
            len: 0,
        }
    }

    /// Adds the CPU with hardware id `id`; returns `false` (and adds nothing) once there are
    /// [`MAX_CPUS`].
    pub(crate) fn push(&mut self, id: u64) -> bool {
        if self.len == MAX_CPUS {
            return false;
        }
        self.ids[self.len] = id;
        self.len += 1;
        true
    }

    /// The ids, in the firmware's order.
    pub(crate) fn as_slice(&self) -> &[u64] {
        &self.ids[..self.len]
    }
}
