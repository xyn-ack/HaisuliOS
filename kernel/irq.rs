// SPDX-License-Identifier: GPL-2.0

//! Hardware interrupts.
//!
//! Drives the legacy 8259 PICs and forwards interrupts to user-space drivers as IPC notifications.
//!
//! The kernel contains no device drivers. A privileged process binds an IRQ line (`irq_bind`); when
//! the line fires, the kernel masks it, acknowledges the PIC and posts a `[PID_KERNEL, KMSG_IRQ,
//! line]` notification to the owner. The driver services its device and re-enables the line with
//! `irq_ack`. Lines without an owner stay masked.
//!
//! Interrupts are only ever enabled in user mode and while the scheduler idles
//! ([`crate::process::schedule`]), never elsewhere in the kernel.

use crate::arch::cpu::Device::{self, Pic1, Pic2, PostCode};
use crate::arch::entry::{stub_address, IRQ_STUBS};
use crate::{idt, process};
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::errno::{EBUSY, EINVAL, EPERM};

pub(crate) const LINES: usize = 16;
/// IDT vector of IRQ line 0; lines 0..15 use vectors 32..47.
const VECTOR_BASE: usize = 32;
/// Line 2 chains the slave PIC to the master; it is never a device.
const CASCADE: u64 = 2;

/// PIC registers (port offsets).
const CMD: u16 = 0;
const DATA: u16 = 1;
const EOI: u8 = 0x20;
const READ_ISR: u8 = 0x0B;

/// The pid owning each line (0 = unbound).
static OWNER: [AtomicU64; LINES] = [const { AtomicU64::new(0) }; LINES];

/// Short delay between PIC initialisation words (write to an unused port).
fn io_wait() {
    PostCode.write8(0, 0);
}

/// Remap the PICs to vectors 32..47 (by default IRQ 0-7 would alias CPU
/// exceptions 8-15), mask every device line and install the IDT gates.
pub(crate) fn init() {
    for (pic, offset, wiring) in [
        (Pic1, VECTOR_BASE as u8, 1 << CASCADE),
        (Pic2, VECTOR_BASE as u8 + 8, CASCADE as u8),
    ] {
        pic.write8(CMD, 0x11); // ICW1: initialise, ICW4 follows.
        io_wait();
        pic.write8(DATA, offset); // ICW2: vector offset.
        io_wait();
        pic.write8(DATA, wiring); // ICW3: master: slave on line 2; slave: its id.
        io_wait();
        pic.write8(DATA, 0x01); // ICW4: 8086 mode.
        io_wait();
    }
    Pic1.write8(DATA, !(1u8 << CASCADE)); // Everything masked but the cascade.
    Pic2.write8(DATA, 0xFF);
    for (line, &stub) in IRQ_STUBS.iter().enumerate() {
        idt::set_handler(VECTOR_BASE + line, stub_address(stub));
    }
}

fn set_masked(line: u64, masked: bool) {
    let (pic, bit) = if line < 8 {
        (Pic1, line)
    } else {
        (Pic2, line - 8)
    };
    let m = pic.read8(DATA);
    pic.write8(
        DATA,
        if masked {
            m | 1 << bit
        } else {
            m & !(1 << bit)
        },
    );
}

/// In-service register of one PIC.
fn in_service(pic: Device) -> u8 {
    pic.write8(CMD, READ_ISR);
    pic.read8(CMD)
}

/// Called (with interrupts disabled) by the IRQ stubs in `arch::entry`.
pub(crate) extern "C" fn irq_dispatch(line: u64) {
    // Spurious interrupts (line 7/15 raised without being in service) must
    // not be acknowledged on their own PIC.
    if line == 7 && in_service(Pic1) & 0x80 == 0 {
        return;
    }
    if line == 15 && in_service(Pic2) & 0x80 == 0 {
        Pic1.write8(CMD, EOI); // The master did see the cascade.
        return;
    }
    // Keep the line quiet until its driver has serviced the device.
    set_masked(line, true);
    if line >= 8 {
        Pic2.write8(CMD, EOI);
    }
    Pic1.write8(CMD, EOI);
    let owner = OWNER[line as usize].load(Ordering::Relaxed);
    if owner != 0 {
        process::irq_notify(owner, line);
    }
}

/// Returns the pid owning `line`, or 0.
pub(crate) fn owner(line: usize) -> u64 {
    OWNER[line].load(Ordering::Relaxed)
}

/// Routes IRQ `line` to the calling, privileged process (the `irq_bind` system call).
pub(crate) fn bind(line: u64) -> i64 {
    if line >= LINES as u64 || line == CASCADE {
        return -EINVAL;
    }
    if !process::current_privileged() {
        return -EPERM;
    }
    let me = process::current_pid();
    let owner = OWNER[line as usize].load(Ordering::Relaxed);
    if owner != 0 && owner != me && process::is_alive(owner) {
        return -EBUSY;
    }
    OWNER[line as usize].store(me, Ordering::Relaxed);
    set_masked(line, false);
    0
}

/// Unmasks `line` once its owner has serviced the device (the `irq_ack` system call).
pub(crate) fn ack(line: u64) -> i64 {
    if line >= LINES as u64 {
        return -EINVAL;
    }
    if owner(line as usize) != process::current_pid() {
        return -EPERM;
    }
    set_masked(line, false);
    0
}

/// Unbind and mask every line owned by `pid` (it is exiting).
pub(crate) fn release_all(pid: u64) {
    for (line, owner) in OWNER.iter().enumerate() {
        if owner.load(Ordering::Relaxed) == pid {
            owner.store(0, Ordering::Relaxed);
            set_masked(line as u64, true);
        }
    }
}
