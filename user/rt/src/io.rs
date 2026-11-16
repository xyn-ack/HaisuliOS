// SPDX-License-Identifier: GPL-2.0

//! Console output.
//!
//! Buffered writing to standard output and standard error, and the printing macros.
//!
//! Standard output and error are served by the console server; the kernel forwards `write(1|2)` to
//! it.

use crate::sys;
use core::fmt;

/// Standard input (read from the console).
pub const STDIN: u64 = 0;
/// Standard output (written to the console).
pub const STDOUT: u64 = 1;
/// Standard error (written to the console).
pub const STDERR: u64 = 2;

#[derive(Clone, Copy)]
/// Where a [`BufWriter`] sends its output.
pub enum Sink {
    /// A file descriptor ([`STDOUT`] or [`STDERR`]).
    Fd(u64),
    /// The kernel debug log, which works even without a console server.
    DebugLog,
}

/// Makes `pgid` the terminal's foreground process group (POSIX `tcsetpgrp`).
///
/// The console server sends the terminal's signals, such as the SIGINT of
/// Ctrl-C, to that group, and interrupts reads from it with `EINTR`. Like
/// reading or writing the terminal, this waits for the console server if it is
/// not there (yet, or again: the process server restarts it if it dies).
pub fn tcsetpgrp(pgid: u64) -> sys::Result<()> {
    let console = sys::svc_wait(crate::abi::svc::CONSOLE)?;
    let mut pgid = pgid.to_ne_bytes();
    sys::ipc_call(console, crate::abi::console::OP_SET_FG_PGRP, &mut pgid).map(drop)
}

/// Writes all of `buf` to `fd`, retrying short writes.
pub fn write_all(fd: u64, mut buf: &[u8]) -> sys::Result<()> {
    while !buf.is_empty() {
        match sys::write(fd, buf)? {
            0 => return Err(sys::Errno(crate::abi::errno::EIO)),
            n => buf = &buf[n.min(buf.len())..],
        }
    }
    Ok(())
}

/// Collects formatted output so that a `print!` costs one IPC round trip
/// instead of one per formatting fragment.
pub struct BufWriter {
    sink: Sink,
    buf: [u8; 256],
    len: usize,
}

impl BufWriter {
    /// Creates an empty writer for `sink`.
    pub const fn new(sink: Sink) -> Self {
        BufWriter {
            sink,
            buf: [0; 256],
            len: 0,
        }
    }

    /// Sends the buffered output to the sink.
    pub fn flush(&mut self) {
        let data = &self.buf[..self.len];
        match self.sink {
            Sink::Fd(fd) => {
                let _ = write_all(fd, data);
            }
            Sink::DebugLog => sys::debug_log(data),
        }
        self.len = 0;
    }
}

impl fmt::Write for BufWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len == self.buf.len() {
                self.flush();
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(fd: u64, args: fmt::Arguments<'_>) {
    let mut w = BufWriter::new(Sink::Fd(fd));
    let _ = fmt::Write::write_fmt(&mut w, args);
    w.flush();
}

/// Prints to standard output.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::io::_print($crate::io::STDOUT, format_args!($($arg)*)) };
}
/// Prints to standard output, with a newline.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
/// Prints to standard error, with a newline.
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {
        $crate::io::_print($crate::io::STDERR, format_args!("{}\n", format_args!($($arg)*)))
    };
}
