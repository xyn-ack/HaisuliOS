// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS console server.
//!
//! Drives the machine's console UART and provides a terminal with line editing to the rest of the
//! system.
//!
//! The server owns the UART: the machine says which device it is (`device_info` for
//! [`CONSOLE`]: a 16550 on a PC, a PL011 on arm64, see [`uart`]), the kernel grants its registers
//! (`device_grant`) and delivers its receive interrupts as IPC notifications. On top of it, the
//! server implements a small canonical-mode line discipline: input is
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
// Device access goes through the runtime's granted registers (`haisuli_rt::device`).
#![forbid(unsafe_code)]

use haisuli_rt::abi::console::{OP_READ, OP_SET_FG_PGRP, OP_WRITE};
use haisuli_rt::abi::device::{CONSOLE, NO_IRQ};
use haisuli_rt::abi::errno::{EAGAIN, EFAULT, EINVAL, ENODEV, ENOSYS};
use haisuli_rt::abi::signal::SIGINT;
use haisuli_rt::abi::{svc, KMSG_IRQ, PID_KERNEL};
use haisuli_rt::{device, sys};

haisuli_rt::entry!(main);

/// Longest input line, including its terminating newline.
const LINE_MAX: usize = 256;

mod uart {
    //! The UART drivers: interrupt-driven input; output goes through the kernel's transmitter
    //! (see [`crate::output`]), so a driver only sets the device up and reads what it received.
    use haisuli_rt::abi::device::{DeviceInfo, KIND_UART_16550, KIND_UART_PL011};
    use haisuli_rt::abi::{pl011, uart16550};
    use haisuli_rt::device::Registers;

    /// The console's UART, with the driver for the kind of device it is.
    pub(crate) enum Uart {
        /// A 16550 (a PC's COM ports).
        Ns16550(Registers),
        /// An ARM PL011.
        Pl011(Registers),
    }

    impl Uart {
        /// The driver for device `info`, whose registers are `regs`; `None` for a device this
        /// server has no driver for.
        pub(crate) fn new(info: &DeviceInfo, regs: Registers) -> Option<Uart> {
            match info.kind {
                KIND_UART_16550 => Some(Uart::Ns16550(regs)),
                KIND_UART_PL011 => Some(Uart::Pl011(regs)),
                _ => None,
            }
        }

        /// Sets the device up: 8N1, an interrupt for received bytes.
        pub(crate) fn init(&self) {
            match self {
                Uart::Ns16550(regs) => ns16550_init(regs),
                Uart::Pl011(regs) => pl011_init(regs),
            }
        }

        /// The next byte received, if there is one.
        pub(crate) fn read_byte(&self) -> Option<u8> {
            match self {
                Uart::Ns16550(regs) => {
                    let status = regs.read8(uart16550::Register::LineStatus as u64);
                    (status & uart16550::LineStatus::DataReady as u8 != 0)
                        .then(|| regs.read8(uart16550::Register::Data as u64))
                }
                Uart::Pl011(regs) => {
                    let flags = regs.read32(pl011::Register::Flag as u64);
                    (flags & pl011::Flag::ReceiveEmpty as u32 == 0)
                        .then(|| regs.read32(pl011::Register::Data as u64) as u8)
                }
            }
        }
    }

    /// Sets up a 16550: 38400 baud, 8N1, an interrupt for every byte received.
    fn ns16550_init(regs: &Registers) {
        use uart16550::{
            FifoControl, InterruptEnable, LineControl, ModemControl, Register, DIVISOR_38400,
        };
        let write = |reg: Register, value: u8| regs.write8(reg as u64, value);
        write(Register::InterruptEnable, 0); // Interrupts off while configuring.
        write(Register::LineControl, LineControl::DivisorLatch as u8);
        let [low, high] = DIVISOR_38400.to_le_bytes();
        write(Register::DIVISOR_LOW, low);
        write(Register::DIVISOR_HIGH, high);
        // 8N1, which also ends DLAB.
        write(Register::LineControl, LineControl::Word8 as u8);
        // FIFOs on, interrupt per received byte. Not `ClearReceive`: anything already received
        // is real (early) typeahead.
        write(Register::FifoControl, FifoControl::Enable as u8);
        // OUT2 connects the UART's interrupt line to the PIC.
        write(
            Register::ModemControl,
            ModemControl::bits(&[
                ModemControl::DataTerminalReady,
                ModemControl::RequestToSend,
                ModemControl::Out2,
            ]),
        );
        write(
            Register::InterruptEnable,
            InterruptEnable::ReceivedData as u8,
        );
    }

    /// Sets up a PL011: its receiver, and an interrupt for received bytes.
    ///
    /// The kernel's log already uses it (8N1, at whatever rate the firmware chose), and may be
    /// sending while this runs: so the UART and its transmitter are left as they are, and only
    /// switched on if they were not.
    fn pl011_init(regs: &Registers) {
        use pl011::{Control, Interrupt, Register};
        let receive = Interrupt::bits(&[Interrupt::Receive, Interrupt::ReceiveTimeout]);
        regs.write32(Register::InterruptClear as u64, receive);
        regs.write32(Register::InterruptMask as u64, receive);
        let control = regs.read32(Register::Control as u64);
        regs.write32(
            Register::Control as u64,
            control | Control::bits(&[Control::Enable, Control::Transmit, Control::Receive]),
        );
    }
}

mod output {
    //! Terminal output, through the kernel's transmitter (`serial_write`): then neither the
    //! kernel's log nor this can cut into the other's output, even on several CPUs at once.

    /// Sends one byte as it is.
    pub(crate) fn write_byte(b: u8) {
        let _ = haisuli_rt::sys::serial_write(&[b]);
    }

    /// Terminal output: newline becomes CR LF. Sent in one piece, in chunks of this many bytes
    /// of output.
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
                output::write(b"\n");
                self.line[self.line_len] = b'\n';
                self.line_len += 1;
                self.finish_line();
            }
            Some(Control::Delete | Control::Backspace) => {
                self.erase_char();
            }
            Some(Control::Kill) => while self.erase_char() {},
            Some(Control::Interrupt) => {
                output::write(b"^C\n");
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
                    output::write_byte(b);
                } else {
                    output::write_byte(Control::Bell as u8); // Line full.
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
        output::write(&ERASE);
        true
    }

    /// Make the edited line readable and hand it to waiting readers.
    fn finish_line(&mut self) {
        let n = self.line_len;
        self.line_len = 0;
        if self.cooked.free() < n || self.lines.free() == 0 {
            output::write_byte(Control::Bell as u8); // Nobody is reading: input dropped.
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
                output::write(&chunk[..n]);
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
    let (info, regs) = match device::grant(CONSOLE) {
        Ok(granted) => granted,
        Err(e) => return fail("device_grant", e),
    };
    let Some(uart) = uart::Uart::new(&info, regs) else {
        return fail("no driver for the console device", sys::Errno(ENODEV));
    };
    if info.irq == NO_IRQ {
        return fail(
            "no interrupt line for the console device",
            sys::Errno(ENODEV),
        );
    }
    uart.init();
    if let Err(e) = sys::irq_bind(info.irq) {
        return fail("irq_bind", e);
    }
    if let Err(e) = sys::svc_register(svc::CONSOLE) {
        return fail("svc_register", e);
    }
    let mut w = haisuli_rt::io::BufWriter::new(haisuli_rt::io::Sink::DebugLog);
    let _ = core::fmt::Write::write_fmt(
        &mut w,
        format_args!("[console] serving {} (IRQ {})\n", info.name(), info.irq),
    );
    w.flush();

    let mut tty = Tty::new();
    // Keys typed before we were running are still in the UART: keep them.
    while let Some(b) = uart.read_byte() {
        tty.input(b);
    }
    loop {
        let m = match sys::ipc_recv() {
            Ok(m) => m,
            Err(e) => return fail("ipc_recv", e),
        };
        match (m.sender, m.a) {
            (PID_KERNEL, KMSG_IRQ) => {
                while let Some(b) = uart.read_byte() {
                    tty.input(b);
                }
                let _ = sys::irq_ack(info.irq);
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
