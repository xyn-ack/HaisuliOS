// SPDX-License-Identifier: GPL-2.0

//! Global descriptor table.
//!
//! Sets up the GDT and the TSS, including each task's I/O-permission bitmap.
//!
//! The layout ([`Selector`]) is dictated by the STAR MSR of SYSCALL/SYSRET: kernel code must be
//! followed by kernel data, and user data by user code.
//!
//! Every CPU has a TSS of its own (the kernel stack for interrupts from user mode, and the
//! double-fault stack), described by its own pair of GDT entries after the segments; they all
//! share the one GDT.
//!
//! The TSS carries an I/O-permission bitmap for ports `0..IO_PORTS`, loaded per task on every
//! context switch: this is how user-space drivers get access to exactly the device registers they
//! were granted (`ioperm`). Ports at or above `IO_PORTS` lie beyond the TSS limit and always fault.

use super::cpu;
use super::percpu::{self, MAX_CPUS};
use super::x86::{
    segment_descriptor, system_descriptor, PrivilegeLevel, SegmentAccess, SegmentFlag,
    SystemSegment,
};
use crate::sync::Lock;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::IO_PORTS;

/// Size of the per-task I/O bitmap: one bit per port, 1 = access denied.
pub(crate) const IO_BITMAP_BYTES: usize = (IO_PORTS / 8) as usize;
pub(crate) type IoBitmap = [u8; IO_BITMAP_BYTES];
/// A bitmap byte denying all of its 8 ports.
const DENY_8_PORTS: u8 = 0xFF;
/// Bitmap denying every port (the default for each task).
pub(crate) const IO_DENY_ALL: IoBitmap = [DENY_8_PORTS; IO_BITMAP_BYTES];

/// The GDT's descriptors, by selector (byte offset in the GDT).
///
/// SYSCALL loads kernel code from STAR and kernel data from the next entry;
/// SYSRET loads user data and then user code from the two entries after
/// another base in STAR. Hence kernel data right after kernel code, and user
/// data right before user code (see [`crate::arch::entry::init_syscalls`]).
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub(crate) enum Selector {
    /// The mandatory null descriptor.
    Null = 0x00,
    /// 64-bit kernel code.
    KernelCode = 0x08,
    /// Kernel data (and stack).
    KernelData = 0x10,
    /// User data (and stack).
    UserData = 0x18,
    /// 64-bit user code.
    UserCode = 0x20,
    /// The boot CPU's TSS, a 16-byte descriptor that takes two entries; the other CPUs' follow
    /// (see [`tss_selector`]).
    Tss = 0x28,
}

/// The selector of CPU `cpu`'s TSS.
const fn tss_selector(cpu: usize) -> u16 {
    Selector::Tss as u16 + (cpu * 2 * Selector::ENTRY_SIZE) as u16
}

impl Selector {
    /// Bytes per GDT entry.
    const ENTRY_SIZE: usize = 8;

    /// The GDT entry the selector refers to.
    pub(crate) const fn index(self) -> usize {
        self as usize / Self::ENTRY_SIZE
    }

    /// The selector value that requests privilege level `rpl`.
    pub(crate) const fn with_rpl(self, rpl: PrivilegeLevel) -> u64 {
        self as u64 | rpl as u64
    }
}

/// The kernel's code segment.
pub(crate) const KERNEL_CS: u64 = Selector::KernelCode.with_rpl(PrivilegeLevel::Kernel);
/// The kernel's data segment.
const KERNEL_DS: u64 = Selector::KernelData.with_rpl(PrivilegeLevel::Kernel);
/// The user code segment, as user mode loads it (RPL 3).
pub(crate) const USER_CS: u64 = Selector::UserCode.with_rpl(PrivilegeLevel::User);
/// The user data and stack segment, as user mode loads it (RPL 3).
pub(crate) const USER_DS: u64 = Selector::UserData.with_rpl(PrivilegeLevel::User);

/// The interrupt-stack-table slot of the double-fault stack (`Tss::ist1`).
pub(crate) const DOUBLE_FAULT_IST: u8 = 1;

#[repr(C, packed)]
struct Tss {
    _r0: u32,
    rsp0: u64,
    _rsp12: [u64; 2],
    _r1: u64,
    ist1: u64,
    _ist: [u64; 6],
    _r2: u64,
    _r3: u16,
    iopb: u16,
    io_bitmap: IoBitmap,
    /// The CPU may read one byte past the bitmap; it must have all bits set.
    io_bitmap_end: u8,
}

/// Where the I/O-permission bitmap starts in the TSS (right after the
/// architectural part, which is 104 bytes).
const IO_BITMAP_OFFSET: u16 = core::mem::offset_of!(Tss, io_bitmap) as u16;
const _: () = assert!(IO_BITMAP_OFFSET == 104);

/// Each CPU's TSS. Only its own CPU changes it, but the lock keeps the `&mut` unique.
static TSS: [Lock<Tss>; MAX_CPUS] = [const {
    Lock::new(Tss {
        _r0: 0,
        rsp0: 0,
        _rsp12: [0; 2],
        _r1: 0,
        ist1: 0,
        _ist: [0; 6],
        _r2: 0,
        _r3: 0,
        iopb: IO_BITMAP_OFFSET,
        io_bitmap: IO_DENY_ALL,
        io_bitmap_end: DENY_8_PORTS,
    })
}; MAX_CPUS];

/// The largest segment limit (20 bits), in 4 KiB units with
/// [`SegmentFlag::Granularity4K`]: the whole address space.
const MAX_LIMIT: u32 = 0xF_FFFF;

/// A flat code segment (readable) for privilege level `dpl`.
pub(crate) const fn code_segment(dpl: PrivilegeLevel) -> u64 {
    let access = SegmentAccess::bits(&[
        SegmentAccess::Present,
        SegmentAccess::CodeOrData,
        SegmentAccess::Executable,
        SegmentAccess::ReadWrite,
    ]);
    let user = match dpl {
        PrivilegeLevel::Kernel => 0,
        PrivilegeLevel::User => SegmentAccess::User as u8,
    };
    let flags = SegmentFlag::bits(&[SegmentFlag::Granularity4K, SegmentFlag::LongMode]);
    segment_descriptor(access | user, flags, MAX_LIMIT)
}

/// A flat data segment (writable) for privilege level `dpl`.
pub(crate) const fn data_segment(dpl: PrivilegeLevel) -> u64 {
    let access = SegmentAccess::bits(&[
        SegmentAccess::Present,
        SegmentAccess::CodeOrData,
        SegmentAccess::ReadWrite,
    ]);
    let user = match dpl {
        PrivilegeLevel::Kernel => 0,
        PrivilegeLevel::User => SegmentAccess::User as u8,
    };
    let flags = SegmentFlag::bits(&[SegmentFlag::Granularity4K, SegmentFlag::Size32]);
    segment_descriptor(access | user, flags, MAX_LIMIT)
}

/// Entries in the GDT: the segments, then two for each CPU's TSS.
const GDT_ENTRIES: usize = Selector::Tss.index() + 2 * MAX_CPUS;

static GDT: [AtomicU64; GDT_ENTRIES] = {
    let mut gdt = [const { AtomicU64::new(0) }; GDT_ENTRIES];
    gdt[Selector::Null.index()] = AtomicU64::new(0);
    gdt[Selector::KernelCode.index()] = AtomicU64::new(code_segment(PrivilegeLevel::Kernel));
    gdt[Selector::KernelData.index()] = AtomicU64::new(data_segment(PrivilegeLevel::Kernel));
    gdt[Selector::UserData.index()] = AtomicU64::new(data_segment(PrivilegeLevel::User));
    gdt[Selector::UserCode.index()] = AtomicU64::new(code_segment(PrivilegeLevel::User));
    // The TSS is filled in by `init`, when its address is known.
    gdt
};

/// Separate stacks for double faults ([`DOUBLE_FAULT_IST`]), one per CPU, used only by the CPU.
#[repr(C, align(16))]
struct Stack([AtomicU64; 1024]);
static DOUBLE_FAULT_STACKS: [Stack; MAX_CPUS] =
    [const { Stack([const { AtomicU64::new(0) }; 1024]) }; MAX_CPUS];

/// Stack this CPU switches to on a user->kernel interrupt/exception.
pub(crate) fn set_kernel_stack(top: u64) {
    TSS[percpu::index()].lock().rsp0 = top;
}

/// Install the I/O permissions of the task about to run on this CPU.
pub(crate) fn set_io_bitmap(bitmap: &IoBitmap) {
    TSS[percpu::index()].lock().io_bitmap = *bitmap;
}

/// Loads the GDT and this CPU's TSS; every CPU calls it once, with its own index.
pub(crate) fn init() {
    let cpu = percpu::index();
    let tss = &TSS[cpu];
    tss.lock().ist1 = DOUBLE_FAULT_STACKS[cpu].0.as_ptr() as u64 + size_of::<Stack>() as u64;

    let limit = (size_of::<Tss>() - 1) as u32;
    let [low, high] = system_descriptor(SystemSegment::AvailableTss64, tss.addr(), limit);
    let index = tss_selector(cpu) as usize / Selector::ENTRY_SIZE;
    GDT[index].store(low, Ordering::Relaxed);
    GDT[index + 1].store(high, Ordering::Relaxed);
    cpu::load_gdt(&GDT, KERNEL_CS as u16, KERNEL_DS as u16, tss_selector(cpu));
}
