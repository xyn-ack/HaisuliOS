// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS console server.
//!
//! Drives the COM1 UART and provides a terminal with line editing to the rest of the system.
//!
//! The server owns the UART (I/O ports granted with `ioperm`, receive interrupts delivered as IPC
//! notifications) and implements a small canonical-mode line discipline on top of it: input is
//! echoed and edited (Backspace, Ctrl-U, Ctrl-C) until Enter, then becomes readable as one line;
//! Ctrl-D on an empty line is end-of-file.
//!
//! Protocol ([`haisuli_rt::abi::console`], usually reached through `read(0)` and `write(1|2)`,
//! which the kernel forwards here):
//!
//! - `OP_WRITE`: output the client's buffer; the reply is the number of bytes written.
//! - `OP_READ`: reply with at most one line, as soon as one is available. Readers wait in a queue;
//!   the server itself never blocks on them.

#![no_std]
#![no_main]
// The only unsafe code is the port I/O in `uart`.
#![deny(unsafe_code)]

use haisuli_rt::abi::console::{OP_READ, OP_WRITE};
use haisuli_rt::abi::errno::{EAGAIN, EFAULT, ENOSYS};
use haisuli_rt::abi::{svc, KMSG_IRQ, PID_KERNEL};
use haisuli_rt::sys;

haisuli_rt::entry!(main);

/// Longest input line, including its terminating newline.
const LINE_MAX: usize = 256;

#[expect(unsafe_code)]
mod uart {
    //! 16550 UART on COM1, polled output, interrupt-driven input.
    use core::arch::asm;

    pub(crate) const BASE: u16 = 0x3F8;
    pub(crate) const PORTS: u64 = 8;
    pub(crate) const IRQ: u64 = 4;
    const LSR_DATA_READY: u8 = 0x01;
    const LSR_THR_EMPTY: u8 = 0x20;

    /// Port of UART register `reg`; only the UART's own 8 registers.
    fn port(reg: u16) -> u16 {
        assert!((reg as u64) < PORTS);
        BASE + reg
    }

    fn outb(reg: u16, v: u8) {
        let port = port(reg);
        // SAFETY: Port I/O on the UART's registers (granted to us by
        // ioperm) touches no memory; the UART does no DMA.
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags))
        };
    }

    fn inb(reg: u16) -> u8 {
        let port = port(reg);
        let v: u8;
        // SAFETY: As in `outb`.
        unsafe {
            asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags))
        };
        v
    }

    pub(crate) fn init() {
        outb(1, 0x00); // Interrupts off while configuring.
        outb(3, 0x80); // DLAB on.
        outb(0, 0x03); // 38400 baud.
        outb(1, 0x00);
        outb(3, 0x03); // 8N1.
                       // FIFO on, interrupt per received byte. Do not set the "clear receive
                       // FIFO" bit: anything already received is real (early) typeahead.
        outb(2, 0x01);
        outb(4, 0x0B); // DTR | RTS | OUT2 (OUT2 gates the IRQ line).
        outb(1, 0x01); // Interrupt on received data.
    }

    pub(crate) fn read_byte() -> Option<u8> {
        (inb(5) & LSR_DATA_READY != 0).then(|| inb(0))
    }

    pub(crate) fn write_byte(b: u8) {
        while inb(5) & LSR_THR_EMPTY == 0 {}
        outb(0, b);
    }

    /// Terminal output: newline becomes CR LF.
    pub(crate) fn write(bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                write_byte(b'\r');
            }
            write_byte(b);
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
                if (0x40..=0x7E).contains(&b) {
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
        match b {
            b'\n' if after_cr => {} // CR LF from the terminal: one newline.
            b'\r' | b'\n' => {
                uart::write(b"\n");
                self.line[self.line_len] = b'\n';
                self.line_len += 1;
                self.finish_line();
            }
            0x7F | 0x08 => {
                self.erase_char(); // Backspace / Delete.
            }
            0x15 => while self.erase_char() {}, // Ctrl-U: kill the line.
            0x03 => {
                // Ctrl-C: no signals yet, so just abandon the line.
                uart::write(b"^C\n");
                self.line_len = 0;
            }
            0x04 => self.finish_line(), // Ctrl-D: flush; on an empty line, EOF.
            0x1B => self.escape = Escape::Esc,
            b if b < 0x20 && b != b'\t' => {} // Other control characters.
            b => {
                if self.line_len < LINE_MAX - 1 {
                    self.line[self.line_len] = b;
                    self.line_len += 1;
                    uart::write_byte(b);
                } else {
                    uart::write_byte(0x07); // Line full: ring the bell.
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
        while self.line_len > 0 && self.line[self.line_len] & 0xC0 == 0x80 {
            self.line_len -= 1; // Continuation bytes of a multi-byte character.
        }
        uart::write(b"\x08 \x08");
        true
    }

    /// Make the edited line readable and hand it to waiting readers.
    fn finish_line(&mut self) {
        let n = self.line_len;
        self.line_len = 0;
        if self.cooked.free() < n || self.lines.free() == 0 {
            uart::write_byte(0x07); // Nobody is reading: input dropped.
            return;
        }
        for &b in &self.line[..n] {
            self.cooked.push(b);
        }
        self.lines.push(n);
        self.serve_readers();
    }

    fn read_request(&mut self, client: u64, capacity: usize) {
        if !self.readers.push(Reader {
            pid: client,
            capacity,
        }) {
            let _ = sys::ipc_reply(client, -EAGAIN);
            return;
        }
        self.serve_readers();
    }

    /// Answer queued readers, oldest first, while complete lines exist.
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
    if let Err(e) = sys::ioperm(uart::BASE as u64, uart::PORTS, true) {
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
            // Unknown request (or a stray notification, for which the reply
            // simply fails).
            (client, _) => {
                let _ = sys::ipc_reply(client, -ENOSYS);
            }
        }
    }
}
