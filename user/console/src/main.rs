// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS console server.
//!
//! Drives the COM1 UART and provides a terminal with line editing to the rest of the system.
//!
//! The server owns the UART (I/O ports granted with `ioperm`, receive interrupts delivered as IPC
//! notifications) and implements a small canonical-mode line discipline on top of it: input is
//! echoed and edited (Backspace, Ctrl-U, Ctrl-C) until Enter, then becomes readable as one line;
//! Ctrl-D on an empty line is end-of-file. The kernel's debug log goes out of the same line, so
//! the server sends its output through the kernel's transmitter (`serial_write`): then neither
//! can cut into the other's output, even when they run on different CPUs at once.
//!
//! Protocol ([`haisuli_rt::abi::console`], usually reached through `read(0)` and `write(1|2)`,
//! which the kernel forwards here):
//!
//! - `OP_WRITE`: output the client's buffer; the reply is the number of bytes written.
//! - `OP_READ`: reply with at most one line, as soon as one is available. Readers wait in a queue;
//!   the server itself never blocks on them.
//! - `OP_SET_FG_PGRP`: set the foreground process group (the `tcsetpgrp` of HaisuliOS).
//!
//! The terminal also delivers signals, which is what makes it a job control terminal: Ctrl-C
//! abandons the line being typed and sends SIGINT to the foreground process group. A process that
//! catches SIGINT (the shell, between jobs) has its pending `read` interrupted by the kernel, which
//! withdraws the call: the server then simply finds it gone (see [`Tty::serve_readers`]).

#![no_std]
#![no_main]
// The only unsafe code is the port I/O in `uart`.
#![deny(unsafe_code)]

use haisuli_rt::abi::console::{OP_READ, OP_SET_FG_PGRP, OP_WRITE};
use haisuli_rt::abi::errno::{EAGAIN, EFAULT, EINVAL, ENOSYS};
use haisuli_rt::abi::signal::SIGINT;
use haisuli_rt::abi::{svc, KMSG_IRQ, PID_KERNEL};
use haisuli_rt::sys;

haisuli_rt::entry!(main);

/// Longest input line, including its terminating newline.
const LINE_MAX: usize = 256;

#[expect(unsafe_code)]
mod uart {
    //! 16550 UART on COM1: interrupt-driven input; output through the kernel's transmitter.
    use core::arch::asm;
    use haisuli_rt::abi::uart::{
        FifoControl, InterruptEnable, LineControl, LineStatus, ModemControl, Register, COM1,
        DIVISOR_38400,
    };
    pub(crate) use haisuli_rt::abi::uart::{COM1 as BASE, COM1_IRQ as IRQ, PORTS};

    /// The port of UART register `reg`: one of the UART's own registers.
    fn port(reg: Register) -> u16 {
        COM1 + reg as u16
    }

    fn outb(reg: Register, v: u8) {
        let port = port(reg);
        // SAFETY: Port I/O on the UART's registers (granted to us by
        // ioperm) touches no memory; the UART does no DMA.
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags))
        };
    }

    fn inb(reg: Register) -> u8 {
        let port = port(reg);
        let v: u8;
        // SAFETY: As in `outb`.
        unsafe {
            asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags))
        };
        v
    }

    /// Sets up COM1: 38400 baud, 8N1, an interrupt for every byte received.
    pub(crate) fn init() {
        outb(Register::InterruptEnable, 0); // Interrupts off while configuring.
        outb(Register::LineControl, LineControl::DivisorLatch as u8);
        let [low, high] = DIVISOR_38400.to_le_bytes();
        outb(Register::DIVISOR_LOW, low);
        outb(Register::DIVISOR_HIGH, high);
        outb(Register::LineControl, LineControl::Word8 as u8); // Also ends DLAB.
                                                               // FIFOs on, interrupt per received byte. Not `ClearReceive`: anything
                                                               // already received is real (early) typeahead.
        outb(Register::FifoControl, FifoControl::Enable as u8);
        // OUT2 connects the UART's interrupt line to the PIC.
        outb(
            Register::ModemControl,
            ModemControl::bits(&[
                ModemControl::DataTerminalReady,
                ModemControl::RequestToSend,
                ModemControl::Out2,
            ]),
        );
        outb(
            Register::InterruptEnable,
            InterruptEnable::ReceivedData as u8,
        );
    }

    fn line_status(status: LineStatus) -> bool {
        inb(Register::LineStatus) & status as u8 != 0
    }

    pub(crate) fn read_byte() -> Option<u8> {
        line_status(LineStatus::DataReady).then(|| inb(Register::Data))
    }

    pub(crate) fn write_byte(b: u8) {
        let _ = haisuli_rt::sys::serial_write(&[b]);
    }

    /// Terminal output: newline becomes CR LF. Sent in one piece (see the module documentation),
    /// in chunks of this many bytes of output.
    pub(crate) fn write(bytes: &[u8]) {
        const CHUNK: usize = 512;
        let mut out = [0u8; CHUNK];
        let mut len = 0;
        for &b in bytes {
            if len + 2 > CHUNK {
                let _ = haisuli_rt::sys::serial_write(&out[..len]);
                len = 0;
            }
            if b == b'\n' {
                out[len] = b'\r';
                len += 1;
            }
            out[len] = b;
            len += 1;
        }
        if len > 0 {
            let _ = haisuli_rt::sys::serial_write(&out[..len]);
        }
    }
}

/// Fixed-capacity FIFO.
struct Fifo<T: Copy, const N: usize> {
    buf: [T; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> Fifo<T, N> {
    const fn new(fill: T) -> Self {
        Fifo {
            buf: [fill; N],
            head: 0,
            len: 0,
        }
    }
    fn free(&self) -> usize {
        N - self.len
    }
    fn push(&mut self, v: T) -> bool {
        if self.len == N {
            return false;
        }
        self.buf[(self.head + self.len) % N] = v;
        self.len += 1;
        true
    }
    fn front(&self) -> Option<&T> {
        (self.len > 0).then(|| &self.buf[self.head])
    }
    fn front_mut(&mut self) -> Option<&mut T> {
        (self.len > 0).then(|| &mut self.buf[self.head])
    }
    fn pop(&mut self) -> Option<T> {
        let v = *self.front()?;
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(v)
    }
    /// The i-th element from the front.
    fn get(&self, i: usize) -> T {
        self.buf[(self.head + i) % N]
    }
}

/// A client blocked in `OP_READ`.
#[derive(Clone, Copy)]
struct Reader {
    pid: u64,
    capacity: usize,
}

/// The ASCII control characters the terminal acts on.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Control {
    /// Ctrl-C (ETX): interrupt.
    Interrupt = 0x03,
    /// Ctrl-D (EOT): end of input (on an empty line), else hand over the line.
    EndOfFile = 0x04,
    /// BEL: output to ring the terminal's bell.
    Bell = 0x07,
    /// Ctrl-H (BS): erase a character; as output, move the cursor left.
    Backspace = 0x08,
    /// LF: end of line.
    LineFeed = 0x0A,
    /// CR (Enter): end of line.
    CarriageReturn = 0x0D,
    /// Ctrl-U (NAK): kill the line being typed.
    Kill = 0x15,
    /// ESC: starts an escape sequence (which is ignored).
    Escape = 0x1B,
    /// DEL, which most terminals send for Backspace: erase a character.
    Delete = 0x7F,
}

impl Control {
    fn from_byte(b: u8) -> Option<Control> {
        use Control::*;
        [
            Interrupt,
            EndOfFile,
            Bell,
            Backspace,
            LineFeed,
            CarriageReturn,
            Kill,
            Escape,
            Delete,
        ]
        .into_iter()
        .find(|&c| c as u8 == b)
    }
}

/// What erasing a character looks like: back, overwrite with a space, back.
const ERASE: [u8; 3] = [Control::Backspace as u8, b' ', Control::Backspace as u8];

/// The final byte of a CSI escape sequence (ECMA-48: 0x40 to 0x7E); the bytes
/// before it are parameters.
const CSI_FINAL: core::ops::RangeInclusive<u8> = b'@'..=b'~';

/// Whether `b` continues a multi-byte UTF-8 character (it is `0b10xx_xxxx`).
fn is_utf8_continuation(b: u8) -> bool {
    b & 0b1100_0000 == 0b1000_0000
}

#[derive(Clone, Copy, PartialEq)]
enum Escape {
    None,
    Esc,
    /// ESC [ ... until a final byte.
    Csi,
    /// ESC O x (keypad / function keys).
    Ss3,
}

struct Tty {
    /// The line being edited.
    line: [u8; LINE_MAX],
    line_len: usize,
    /// The bytes of all completed lines of input.
    ///
    /// `lines` holds the length of each line still (partly) unread; a
    /// 0-length line is end-of-file.
    cooked: Fifo<u8, 1024>,
    lines: Fifo<usize, 32>,
    readers: Fifo<Reader, 16>,
    escape: Escape,
    after_cr: bool,
    /// The process group that receives the terminal's signals (0 = none).
    fg_pgrp: u64,
}

impl Tty {
    const fn new() -> Self {
        Tty {
            line: [0; LINE_MAX],
            line_len: 0,
            cooked: Fifo::new(0),
            lines: Fifo::new(0),
            readers: Fifo::new(Reader {
                pid: 0,
                capacity: 0,
            }),
            escape: Escape::None,
            after_cr: false,
            fg_pgrp: 0,
        }
    }

    /// Line discipline: one received byte.
    fn input(&mut self, b: u8) {
        let after_cr = core::mem::replace(&mut self.after_cr, b == b'\r');
        match self.escape {
            Escape::Esc => {
                self.escape = match b {
                    b'[' => Escape::Csi,
                    b'O' => Escape::Ss3,
                    _ => Escape::None,
                };
                return;
            }
            Escape::Csi => {
                if CSI_FINAL.contains(&b) {
                    self.escape = Escape::None;
                }
                return;
            }
            Escape::Ss3 => {
                self.escape = Escape::None;
                return;
            }
            Escape::None => {}
        }
        match Control::from_byte(b) {
            // CR LF from the terminal: one newline.
            Some(Control::LineFeed) if after_cr => {}
            Some(Control::CarriageReturn | Control::LineFeed) => {
                uart::write(b"\n");
                self.line[self.line_len] = b'\n';
                self.line_len += 1;
                self.finish_line();
            }
            Some(Control::Delete | Control::Backspace) => {
                self.erase_char();
            }
            Some(Control::Kill) => while self.erase_char() {},
            Some(Control::Interrupt) => {
                uart::write(b"^C\n");
                self.line_len = 0;
                self.interrupt();
            }
            Some(Control::EndOfFile) => self.finish_line(), // On an empty line: EOF.
            Some(Control::Escape) => self.escape = Escape::Esc,
            // Other control characters (and BEL) do nothing; a tab is text.
            Some(Control::Bell) | None if b.is_ascii_control() && b != b'\t' => {}
            Some(Control::Bell) | None => {
                if self.line_len < LINE_MAX - 1 {
                    self.line[self.line_len] = b;
                    self.line_len += 1;
                    uart::write_byte(b);
                } else {
                    uart::write_byte(Control::Bell as u8); // Line full.
                }
            }
        }
    }

    /// Remove the last (UTF-8) character of the line being edited.
    fn erase_char(&mut self) -> bool {
        if self.line_len == 0 {
            return false;
        }
        self.line_len -= 1;
        while self.line_len > 0 && is_utf8_continuation(self.line[self.line_len]) {
            self.line_len -= 1; // The rest of a multi-byte character.
        }
        uart::write(&ERASE);
        true
    }

    /// Make the edited line readable and hand it to waiting readers.
    fn finish_line(&mut self) {
        let n = self.line_len;
        self.line_len = 0;
        if self.cooked.free() < n || self.lines.free() == 0 {
            uart::write_byte(Control::Bell as u8); // Nobody is reading: input dropped.
            return;
        }
        for &b in &self.line[..n] {
            self.cooked.push(b);
        }
        self.lines.push(n);
        self.serve_readers();
    }

    /// Ctrl-C: send SIGINT to the foreground process group.
    ///
    /// The line being edited is already discarded. The signal terminates a
    /// foreground job that does not catch it; a process that does (the shell)
    /// runs its handler, and a read it was blocked in fails with `EINTR`.
    fn interrupt(&mut self) {
        if self.fg_pgrp != 0 {
            // Not `kill(-pgrp)`: between jobs the foreground group is the
            // shell's, group 1, and `kill(-1)` would signal every process.
            let _ = sys::kill_pgrp(self.fg_pgrp, SIGINT);
        }
    }

    /// OP_SET_FG_PGRP: take the process group from the client's buffer.
    fn set_fg_pgrp(&mut self, client: u64) {
        let mut buf = [0u8; 8];
        let status = match sys::ipc_read(client, 0, &mut buf) {
            Ok(8) => {
                self.fg_pgrp = u64::from_ne_bytes(buf);
                0
            }
            Ok(_) => -EINVAL,
            Err(e) => -e.0,
        };
        let _ = sys::ipc_reply(client, status);
    }

    fn read_request(&mut self, client: u64, capacity: usize) {
        // A process has one call at a time: an older read of this client was
        // withdrawn (a signal interrupted it), so forget it.
        self.forget_reader(client);
        if !self.readers.push(Reader {
            pid: client,
            capacity,
        }) {
            let _ = sys::ipc_reply(client, -EAGAIN);
            return;
        }
        self.serve_readers();
    }

    /// Removes `pid` from the queue of readers.
    fn forget_reader(&mut self, pid: u64) {
        let mut keep = Fifo::new(Reader {
            pid: 0,
            capacity: 0,
        });
        while let Some(reader) = self.readers.pop() {
            if reader.pid != pid {
                keep.push(reader);
            }
        }
        self.readers = keep;
    }

    /// Answer queued readers, oldest first, while complete lines exist.
    ///
    /// A reader whose call is gone (it was interrupted by a signal, or the
    /// reader exited) cannot receive the line: the copy fails, and the line
    /// stays for the next reader.
    fn serve_readers(&mut self) {
        while let (Some(&reader), Some(&line)) = (self.readers.front(), self.lines.front()) {
            self.readers.pop();
            let n = line.min(reader.capacity);
            let mut data = [0u8; LINE_MAX];
            for (i, d) in data[..n].iter_mut().enumerate() {
                *d = self.cooked.get(i);
            }
            let status = match sys::ipc_write(reader.pid, 0, &data[..n]) {
                Ok(_) => n as i64,
                Err(_) => {
                    let _ = sys::ipc_reply(reader.pid, -EFAULT);
                    continue; // Leave the input for the next reader.
                }
            };
            // Consume what was delivered; the rest of a long line stays.
            for _ in 0..n {
                self.cooked.pop();
            }
            let rest = self.lines.front_mut().unwrap();
            *rest -= n;
            if *rest == 0 {
                self.lines.pop();
            }
            let _ = sys::ipc_reply(reader.pid, status);
        }
    }
}

/// OP_WRITE: copy the client's buffer out in chunks and send it to the UART.
fn write_request(client: u64, len: usize) {
    let mut chunk = [0u8; 256];
    let mut done = 0;
    let mut status = 0;
    while done < len {
        match sys::ipc_read(client, done, &mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                uart::write(&chunk[..n]);
                done += n;
            }
            Err(e) => {
                status = -e.0;
                break;
            }
        }
    }
    let _ = sys::ipc_reply(client, if done > 0 { done as i64 } else { status });
}

fn fail(what: &str, e: sys::Errno) -> i32 {
    let mut w = haisuli_rt::io::BufWriter::new(haisuli_rt::io::Sink::DebugLog);
    let _ = core::fmt::Write::write_fmt(
        &mut w,
        format_args!("[console] {} failed: errno {}\n", what, e.0),
    );
    w.flush();
    1
}

fn main(_: haisuli_rt::StartInfo) -> i32 {
    if let Err(e) = sys::ioperm(uart::BASE as u64, uart::PORTS as u64, true) {
        return fail("ioperm", e);
    }
    uart::init();
    if let Err(e) = sys::irq_bind(uart::IRQ) {
        return fail("irq_bind", e);
    }
    if let Err(e) = sys::svc_register(svc::CONSOLE) {
        return fail("svc_register", e);
    }
    sys::debug_log(b"[console] serving COM1 (IRQ 4)\n");

    let mut tty = Tty::new();
    // Keys typed before we were running are still in the UART: keep them.
    while let Some(b) = uart::read_byte() {
        tty.input(b);
    }
    loop {
        let m = match sys::ipc_recv() {
            Ok(m) => m,
            Err(e) => return fail("ipc_recv", e),
        };
        match (m.sender, m.a) {
            (PID_KERNEL, KMSG_IRQ) => {
                while let Some(b) = uart::read_byte() {
                    tty.input(b);
                }
                let _ = sys::irq_ack(uart::IRQ);
            }
            (PID_KERNEL, _) => {}
            (client, OP_WRITE) => write_request(client, m.b as usize),
            (client, OP_READ) => tty.read_request(client, m.b as usize),
            (client, OP_SET_FG_PGRP) => tty.set_fg_pgrp(client),
            // Unknown request (or a stray notification, for which the reply
            // simply fails).
            (client, _) => {
                let _ = sys::ipc_reply(client, -ENOSYS);
            }
        }
    }
}
