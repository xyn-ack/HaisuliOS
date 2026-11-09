// SPDX-License-Identifier: GPL-2.0

//! Serial debug log.
//!
//! Writes the kernel's debug log to the COM1 UART and provides the `print!` and `println!` macros.
//!
//! The kernel only ever writes to the UART; input belongs to the user-space console server.

use crate::arch::cpu::Device::Com1;
use core::fmt::{self, Write};

pub(crate) fn init() {
    Com1.write8(1, 0x00); // No interrupts.
    Com1.write8(3, 0x80); // DLAB on.
    Com1.write8(0, 0x03); // 38400 baud.
    Com1.write8(1, 0x00);
    Com1.write8(3, 0x03); // 8N1.
    Com1.write8(2, 0xC7); // FIFO.
    Com1.write8(4, 0x0B);
}

pub(crate) fn write_byte(b: u8) {
    while Com1.read8(5) & 0x20 == 0 {}
    Com1.write8(0, b);
}

pub(crate) fn write_bytes(s: &[u8]) {
    for &b in s {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}

pub(crate) struct Serial;
impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_bytes(s.as_bytes());
        Ok(())
    }
}

pub(crate) fn _print(args: fmt::Arguments<'_>) {
    let _ = Serial.write_fmt(args);
}

/// Prints to the kernel debug log.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::serial::_print(format_args!($($arg)*)) };
}
/// Prints to the kernel debug log, with a newline.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
