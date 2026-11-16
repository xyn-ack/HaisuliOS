// SPDX-License-Identifier: GPL-2.0

//! Serial debug log.
//!
//! Writes the kernel's debug log to the COM1 UART and provides the `print!` and `println!` macros.
//!
//! The kernel only ever writes to the UART; input belongs to the user-space console server.
//!
//! Every CPU logs: a lock keeps each message (a `println!`, or one `debug_log` call) in one
//! piece. The console server sends the terminal's output through here too (`serial_write`), so
//! that the line has one transmitter, and log lines never mix with terminal output.

use crate::arch::cpu::Device::Com1;
use crate::sync::Lock;
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

/// The UART, for one message at a time.
pub(crate) struct Serial;

static LOG: Lock<Serial> = Lock::new(Serial);

/// The log, for a message written in several pieces.
pub(crate) fn lock() -> crate::sync::Guard<'static, Serial> {
    LOG.lock()
}

impl Serial {
    fn write_byte(&mut self, b: u8) {
        while Com1.read8(Register::LineStatus as u16) & LineStatus::TransmitterEmpty as u8 == 0 {}
        write(Register::Data, b);
    }

    /// Writes `s` as it is.
    pub(crate) fn write_raw(&mut self, s: &[u8]) {
        for &b in s {
            self.write_byte(b);
        }
    }

    /// Writes `s`, turning `\n` into `\r\n`.
    pub(crate) fn write_bytes(&mut self, s: &[u8]) {
        for &b in s {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
    }
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

pub(crate) fn _print(args: fmt::Arguments<'_>) {
    let _ = LOG.lock().write_fmt(args);
}

/// Prints for the panic handler, which may have interrupted a message on this CPU (then the
/// lock is held): it writes anyway rather than wait forever.
pub(crate) fn panic_print(args: fmt::Arguments<'_>) {
    match LOG.try_lock() {
        Some(mut log) => {
            let _ = log.write_fmt(args);
        }
        None => {
            let _ = Serial.write_fmt(args);
        }
    }
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
