// SPDX-License-Identifier: MIT

//! QEMU's `virt` machine, for arm64.
//!
//! What [`crate::platform`] re-exports, for QEMU's `virt` machine (`-M virt,gic-version=3`):
//! [`Virt`] implements the contract's traits with the device tree, the PL011 UART, the GICv3,
//! the generic timer and PSCI. Generic code never names the modules below.
//!
//! The machine is booted as Linux is ([`crate::arch::aarch64::boot`]): the loader passes the
//! device tree, which describes the memory, the CPUs, the devices and the initrd. The initrd
//! holds the boot modules, a ustar archive of them in order (init, the process server, the
//! ramdisk), since the Linux boot protocol has one initrd only.
//!
//! Everything the loader left (the device tree, the initrd) stays reserved for the kernel's
//! lifetime (see [`BootInfo::boot_data`]): the allocator never gets it and
//! [`crate::arch::phys`] refuses it, so it is never written after boot and can be handed out as
//! `&'static [u8]`, through the linear map.

mod fdt;
pub(crate) mod gic;
pub(crate) mod pl011;

use super::{BootInfo, CpuIds, Device, InterruptController, Machine, Module, Registers};
use crate::arch::aarch64::{phys, psci, timer};
use crate::arch::{Cpu, Processor};
use core::ops::Range;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use fdt::Fdt;
use haisuli_abi::device::{CONSOLE, KIND_UART_PL011};
use haisuli_abi::pl011::REGISTERS;

/// QEMU's `virt` machine.
pub(crate) struct Virt;

pub(crate) use Virt as Board;

/// Most memory regions, boot-loader ranges and CPUs the description keeps.
const MAX_RANGES: usize = 8;
/// The largest device tree the kernel reads.
const MAX_FDT_SIZE: usize = 1 << 20;
/// The SPI type in an `interrupts` property (the first of its three cells).
const SPI: u64 = 0;

/// A list of physical ranges, filled once at boot.
struct Ranges {
    ranges: [[AtomicU64; 2]; MAX_RANGES],
    len: AtomicUsize,
}

impl Ranges {
    const fn new() -> Ranges {
        Ranges {
            ranges: [const { [AtomicU64::new(0), AtomicU64::new(0)] }; MAX_RANGES],
            len: AtomicUsize::new(0),
        }
    }

    fn push(&self, range: Range<u64>) {
        let i = self.len.load(Ordering::Relaxed);
        if i < MAX_RANGES && range.start < range.end {
            self.ranges[i][0].store(range.start, Ordering::Relaxed);
            self.ranges[i][1].store(range.end, Ordering::Relaxed);
            self.len.store(i + 1, Ordering::Release);
        }
    }

    fn iter(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.ranges[..self.len.load(Ordering::Acquire)]
            .iter()
            .map(|[start, end]| start.load(Ordering::Relaxed)..end.load(Ordering::Relaxed))
    }
}

/// What the device tree says, read once at boot ([`boot`]).
static MEMORY: Ranges = Ranges::new();
static BOOT_DATA: Ranges = Ranges::new();
static INITRD: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static CPUS: Ranges = Ranges::new(); // Each CPU's MPIDR, as `start` of a one-element range.
static UART: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(u64::MAX)]; // Base, SPI.

/// `len` bytes of the loader's memory at physical `addr`, through the linear map.
///
/// # Safety
///
/// The range must be RAM that the loader filled and that nothing writes while the kernel runs
/// (see the module documentation).
unsafe fn loader_bytes(addr: u64, len: usize) -> &'static [u8] {
    // SAFETY: As the caller guarantees; the linear map covers all of `MAX_PHYS`.
    unsafe { core::slice::from_raw_parts(phys::virt(addr) as *const u8, len) }
}

/// A number of one or two cells in a property.
fn cells_value(value: &[u8]) -> Option<u64> {
    fdt::cells(value, 0, (value.len() / 4) as u32)
}

/// Reads the device tree at physical `device_tree`, which the boot loader passed; called by the
/// boot entry, before anything else of the platform.
///
/// Without a (valid) device tree, the machine has no memory map: `kmain` stops there.
pub(crate) fn boot(device_tree: u64) {
    if device_tree == 0 || device_tree + 8 > phys::MAX_PHYS {
        return;
    }
    // SAFETY: The loader passed the device tree there; its header is read first, to know its
    // size, and all of it is reserved (`BOOT_DATA`) below.
    let Some(size) = fdt::total_size(unsafe { loader_bytes(device_tree, 8) }) else {
        return;
    };
    if size > MAX_FDT_SIZE || device_tree + size as u64 > phys::MAX_PHYS {
        return;
    }
    // SAFETY: As above, for all of it.
    let Some(fdt) = Fdt::new(unsafe { loader_bytes(device_tree, size) }) else {
        return;
    };
    BOOT_DATA.push(device_tree..device_tree + size as u64);
    for (addr, size) in fdt.reserved() {
        BOOT_DATA.push(addr..addr.saturating_add(size));
    }
    let mut gic = None;
    fdt.nodes(|node| {
        if node.depth == 1 && node.device_type == b"memory\0" {
            for (addr, size) in (0..).map_while(|n| node.reg(n)) {
                MEMORY.push(addr..addr.saturating_add(size));
            }
        } else if node.depth == 2 && node.parent == b"cpus" && node.device_type == b"cpu\0" {
            if let Some(mpidr) = cells_value(node.reg) {
                CPUS.push(mpidr..mpidr + 1);
            }
        } else if node.depth == 1 && node.name == b"chosen" {
            if let (Some(start), Some(end)) =
                (cells_value(node.initrd_start), cells_value(node.initrd_end))
            {
                INITRD[0].store(start, Ordering::Relaxed);
                INITRD[1].store(end, Ordering::Relaxed);
                BOOT_DATA.push(start..end);
            }
        } else if node.depth == 1 && node.name == b"psci" {
            psci::set_conduit(if node.method == b"smc\0" {
                psci::Conduit::Smc
            } else {
                psci::Conduit::Hvc
            });
        } else if node.is_compatible(b"arm,pl011") && UART[0].load(Ordering::Relaxed) == 0 {
            if let Some((base, _)) = node.reg(0) {
                UART[0].store(base, Ordering::Relaxed);
                pl011::set_base(base);
                if let (Some(SPI), Some(line)) = (
                    fdt::cells(node.interrupts, 0, 1),
                    fdt::cells(node.interrupts, 4, 1),
                ) {
                    UART[1].store(line, Ordering::Relaxed);
                }
            }
        } else if node.is_compatible(b"arm,gic-v3") {
            gic = node.reg(0).zip(node.reg(1));
        }
    });
    if let Some(((distributor, _), (redistributors, size))) = gic {
        gic::set_base(distributor, redistributors, size);
    }
}

/// The PL011 for the log, the generic timer for delays, the CPUs of the device tree and PSCI.
impl Machine for Virt {
    fn console_init() {
        pl011::init();
    }

    fn console_write(byte: u8) {
        pl011::write_byte(byte);
    }

    fn wait_us(us: u32) {
        timer::wait_us(us);
    }

    fn cpus() -> CpuIds {
        let mut ids = CpuIds::new();
        for cpu in CPUS.iter() {
            ids.push(cpu.start);
        }
        ids
    }

    /// PSCI `SYSTEM_OFF` (QEMU then exits with status 0, whatever `code` is).
    fn power_off(_code: u32) -> ! {
        psci::system_off();
        Cpu::halt()
    }

    /// The console is the PL011 that the device tree describes.
    fn device(id: u64) -> Option<Device> {
        match id {
            CONSOLE => {
                let base = UART[0].load(Ordering::Relaxed);
                let line = UART[1].load(Ordering::Relaxed);
                (base != 0).then_some(Device {
                    kind: KIND_UART_PL011,
                    registers: Registers::Mmio {
                        base,
                        len: REGISTERS,
                    },
                    irq: (line < gic::LINES as u64).then_some(line as usize),
                    name: "UART0",
                })
            }
            _ => None,
        }
    }
}

/// The device tree: the memory, the loader's data, and the boot modules in the initrd.
impl BootInfo for Virt {
    fn has_memory_map() -> bool {
        MEMORY.iter().next().is_some()
    }

    fn memory() -> impl Iterator<Item = Range<u64>> {
        MEMORY.iter()
    }

    fn boot_data() -> impl Iterator<Item = Range<u64>> {
        BOOT_DATA.iter()
    }

    fn modules() -> impl Iterator<Item = Module> {
        let (start, end) = (
            INITRD[0].load(Ordering::Relaxed),
            INITRD[1].load(Ordering::Relaxed),
        );
        let bundle: &'static [u8] = if start < end && end <= phys::MAX_PHYS {
            // SAFETY: The loader put the initrd there, and it is reserved (`BOOT_DATA`).
            unsafe { loader_bytes(start, (end - start) as usize) }
        } else {
            &[]
        };
        ustar_files(bundle)
    }
}

/// The files of the ustar archive `archive`, in order: the boot modules.
fn ustar_files(archive: &'static [u8]) -> impl Iterator<Item = Module> {
    const BLOCK: usize = 512;
    let mut at = 0;
    core::iter::from_fn(move || loop {
        let header = archive.get(at..at + BLOCK)?;
        let name = &header[..100];
        let name = &name[..name.iter().position(|&c| c == 0).unwrap_or(100)];
        if name.is_empty() {
            return None; // The end of the archive.
        }
        let size = header[124..136]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .fold(0usize, |n, &c| n * 8 + (c - b'0') as usize);
        let data = archive.get(at + BLOCK..at + BLOCK + size)?;
        at += BLOCK + size.next_multiple_of(BLOCK);
        // Regular files only (type '0', or NUL in old archives).
        if matches!(header[156], b'0' | 0) {
            let name = name.rsplit(|&c| c == b'/').next().unwrap_or(name);
            return Some(Module { data, name });
        }
    })
}

/// The GICv3: its distributor for the lines, each CPU's redistributor for the CPU's interrupts.
impl InterruptController for Virt {
    const LINES: usize = gic::LINES;

    fn init() {
        gic::init();
    }

    fn init_cpu() {
        gic::init_cpu();
    }

    fn is_reserved(_line: usize) -> bool {
        false
    }

    fn mask(line: usize) {
        gic::mask(line);
    }

    fn unmask(line: usize) {
        gic::unmask(line);
    }

    fn acknowledge(line: usize) -> bool {
        gic::acknowledge(line)
    }
}
