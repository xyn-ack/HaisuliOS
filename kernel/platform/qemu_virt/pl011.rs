// SPDX-License-Identifier: MIT

//! The serial line's transmitter.
//!
//! The machine's PL011 UART, write-only: the kernel's log goes out of it, and input belongs to the
//! user-space console server, which drives the rest of the UART itself (through a device grant).
//!
//! The registers are reached through the kernel's linear map, where device memory is mapped
//! uncached ([`crate::arch::aarch64::phys::virt`]); only the PL011's own registers, at their
//! fixed offsets, are accessed, and it does no DMA.

use crate::arch::aarch64::phys;
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::pl011::{Control, Flag, LineControl, Register, REGISTERS};

/// The UART's physical address: QEMU `virt`'s, until the device tree says (see [`set_base`]).
static BASE: AtomicU64 = AtomicU64::new(0x0900_0000);

/// Uses the UART at physical address `base` (its register block is [`REGISTERS`] bytes, page
/// aligned, in the device part of the linear map).
pub(crate) fn set_base(base: u64) {
    BASE.store(base, Ordering::Relaxed);
}

fn address(reg: Register) -> *mut u32 {
    let offset = reg as u64;
    debug_assert!(offset < REGISTERS);
    phys::virt(BASE.load(Ordering::Relaxed) + offset) as *mut u32
}

fn read(reg: Register) -> u32 {
    // SAFETY: `address` is one of the PL011's 32-bit registers, mapped uncached in the linear
    // map (see the module documentation); reading it has no effect on memory.
    unsafe { core::ptr::read_volatile(address(reg)) }
}

fn write(reg: Register, value: u32) {
    // SAFETY: As in `read`; the UART cannot access memory.
    unsafe { core::ptr::write_volatile(address(reg), value) }
}

/// Sets the UART up for output: 8N1 with FIFOs, transmitter and receiver enabled (the baud rate
/// stays the firmware's).
pub(crate) fn init() {
    write(
        Register::LineControl,
        LineControl::bits(&[LineControl::Word8, LineControl::Fifos]),
    );
    write(
        Register::Control,
        Control::bits(&[Control::Enable, Control::Transmit, Control::Receive]),
    );
}

/// Sends one byte, waiting until the transmit FIFO takes it.
pub(crate) fn write_byte(b: u8) {
    while read(Register::Flag) & Flag::TransmitFull as u32 != 0 {
        core::hint::spin_loop();
    }
    write(Register::Data, b as u32);
}
