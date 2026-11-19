// SPDX-License-Identifier: MIT

//! Serial debug log.
//!
//! Writes the kernel's debug log to the platform's serial line and provides the `print!` and
//! `println!` macros.
//!
//! The kernel only ever writes to the line; input belongs to the user-space console server.
//!
//! Every CPU logs: a lock keeps each message (a `println!`, or one `debug_log` call) in one
//! piece. The console server sends the terminal's output through here too (`serial_write`), so
//! that the line has one transmitter, and log lines never mix with terminal output.

use crate::platform::{Board, Machine};
use crate::sync::Lock;
use core::fmt::{self, Write};

/// Sets the serial line up for output.
pub(crate) fn init() {
    Board::console_init();
}

/// The serial line (the platform's early console), for one message at a time.
pub(crate) struct Serial;

static LOG: Lock<Serial> = Lock::new(Serial);

/// The log, for a message written in several pieces.
pub(crate) fn lock() -> crate::sync::Guard<'static, Serial> {
    LOG.lock()
}

impl Serial {
    fn write_byte(&mut self, b: u8) {
        Board::console_write(b);
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
