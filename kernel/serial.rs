// SPDX-License-Identifier: GPL-2.0

//! Serial debug log.
//!
//! Writes the kernel's debug log to the COM1 UART and provides the `print!` and `println!` macros.
//!
//! The kernel only ever writes to the UART; input belongs to the user-space console server.

use crate::arch::cpu::Device::Com1;
use core::fmt::{self, Write};
use haisuli_abi::uart::{
    FifoControl, LineControl, LineStatus, ModemControl, Register, DIVISOR_38400,
};

fn write(reg: Register, value: u8) {
    Com1.write8(reg as u16, value);
}

/// Sets up COM1 for output: 38400 baud, 8N1, FIFOs on, no interrupts.
pub(crate) fn init() {
    write(Register::InterruptEnable, 0);
    write(Register::LineControl, LineControl::DivisorLatch as u8);
    let [low, high] = DIVISOR_38400.to_le_bytes();
    write(Register::DIVISOR_LOW, low);
    write(Register::DIVISOR_HIGH, high);
    write(Register::LineControl, LineControl::Word8 as u8); // Also ends DLAB.
    write(
        Register::FifoControl,
        FifoControl::bits(&[
            FifoControl::Enable,
            FifoControl::ClearReceive,
            FifoControl::ClearTransmit,
            FifoControl::Trigger14,
        ]),
    );
    write(
        Register::ModemControl,
        ModemControl::bits(&[
            ModemControl::DataTerminalReady,
            ModemControl::RequestToSend,
            ModemControl::Out2,
        ]),
    );
}

pub(crate) fn write_byte(b: u8) {
    while Com1.read8(Register::LineStatus as u16) & LineStatus::TransmitterEmpty as u8 == 0 {}
    write(Register::Data, b);
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
