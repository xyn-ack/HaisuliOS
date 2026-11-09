// SPDX-License-Identifier: GPL-2.0

//! Global descriptor table.
//!
//! Sets up the GDT and the TSS, including each process's I/O-permission bitmap.
//!
//! The layout is dictated by the STAR MSR of SYSCALL/SYSRET:
//!
//! ```text
//! 0x00 null | 0x08 kernel code | 0x10 kernel data
//! 0x18 user data | 0x20 user code | 0x28 TSS (16 bytes)
//! ```
//!
//! The TSS carries an I/O-permission bitmap for ports `0..IO_PORTS`, loaded per process on every
//! context switch: this is how user-space drivers get access to exactly the device registers they
//! were granted (`ioperm`). Ports at or above `IO_PORTS` lie beyond the TSS limit and always fault.

use crate::arch::cpu;
use crate::sync::Lock;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::IO_PORTS;

/// Size of the per-process I/O bitmap: one bit per port, 1 = access denied.
pub(crate) const IO_BITMAP_BYTES: usize = (IO_PORTS / 8) as usize;
pub(crate) type IoBitmap = [u8; IO_BITMAP_BYTES];
/// Bitmap denying every port (the default for each process).
pub(crate) const IO_DENY_ALL: IoBitmap = [0xFF; IO_BITMAP_BYTES];

pub(crate) const KERNEL_CS: u64 = 0x08;
pub(crate) const USER_DS: u64 = 0x18 | 3;
pub(crate) const USER_CS: u64 = 0x20 | 3;

const KERNEL_DS: u64 = 0x10;
const TSS_SELECTOR: u64 = 0x28;

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

const _: () = assert!(core::mem::offset_of!(Tss, io_bitmap) == 104);

static TSS: Lock<Tss> = Lock::new(Tss {
    _r0: 0,
    rsp0: 0,
    _rsp12: [0; 2],
    _r1: 0,
    ist1: 0,
    _ist: [0; 6],
    _r2: 0,
    _r3: 0,
    iopb: 104, // The offset of `io_bitmap`.
    io_bitmap: IO_DENY_ALL,
    io_bitmap_end: 0xFF,
});

static GDT: [AtomicU64; 7] = [
    AtomicU64::new(0),
    AtomicU64::new(0x00AF_9A00_0000_FFFF), // Kernel code, 64-bit.
    AtomicU64::new(0x00CF_9200_0000_FFFF), // Kernel data.
    AtomicU64::new(0x00CF_F200_0000_FFFF), // User data.
    AtomicU64::new(0x00AF_FA00_0000_FFFF), // User code, 64-bit.
    AtomicU64::new(0),                     // TSS (16 bytes, filled in by `init`).
    AtomicU64::new(0),
];

/// Separate stack for double faults (IST 1), used only by the CPU.
#[repr(C, align(16))]
struct Stack([AtomicU64; 1024]);
static DOUBLE_FAULT_STACK: Stack = Stack([const { AtomicU64::new(0) }; 1024]);

/// Stack the CPU switches to on a user->kernel interrupt/exception.
pub(crate) fn set_kernel_stack(top: u64) {
    TSS.lock().rsp0 = top;
}

/// Install the I/O permissions of the process about to run.
pub(crate) fn set_io_bitmap(bitmap: &IoBitmap) {
    TSS.lock().io_bitmap = *bitmap;
}

pub(crate) fn init() {
    TSS.lock().ist1 = DOUBLE_FAULT_STACK.0.as_ptr() as u64 + size_of::<Stack>() as u64;

    let base = TSS.addr();
    let limit = (size_of::<Tss>() - 1) as u64;
    GDT[5].store(
        (limit & 0xFFFF)
            | ((base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40) // Present, 64-bit available TSS.
            | (((limit >> 16) & 0xF) << 48)
            | (((base >> 24) & 0xFF) << 56),
        Ordering::Relaxed,
    );
    GDT[6].store(base >> 32, Ordering::Relaxed);
    cpu::load_gdt(
        &GDT,
        KERNEL_CS as u16,
        KERNEL_DS as u16,
        TSS_SELECTOR as u16,
    );
}
