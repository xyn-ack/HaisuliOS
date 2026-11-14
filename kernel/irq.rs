// SPDX-License-Identifier: GPL-2.0

//! Hardware interrupts.
//!
//! Drives the legacy 8259 PICs and forwards interrupts to user-space drivers as IPC notifications.
//!
//! The kernel contains no device drivers. A privileged task binds an IRQ line (`irq_bind`); when
//! the line fires, the kernel masks it, acknowledges the PIC and posts a `[PID_KERNEL, KMSG_IRQ,
//! line]` notification to the owner. The driver services its device and re-enables the line with
//! `irq_ack`. Lines without an owner stay masked.
//!
//! Interrupts are only ever enabled in user mode and while the scheduler idles
//! ([`crate::task::schedule`]), never elsewhere in the kernel.

use crate::arch::cpu::Device::{self, Pic1, Pic2, PostCode};
use crate::arch::entry::{stub_address, IRQ_STUBS};
use crate::arch::x86::Exception;
use crate::{idt, task};
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::errno::{EBUSY, EINVAL, EPERM};

/// Lines of each 8259.
const LINES_PER_PIC: u64 = 8;
/// Lines of the master and slave PIC together.
pub(crate) const LINES: usize = 2 * LINES_PER_PIC as usize;
/// IDT vector of IRQ line 0: the first after the CPU's exception vectors, so
/// lines 0..15 use vectors 32..47.
const VECTOR_BASE: usize = Exception::VECTORS;

/// IRQ lines with a fixed role in the master/slave pair of 8259s.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
enum Line {
    /// The master's line 2, which the slave is wired to: never a device.
    Cascade = 2,
    /// The master's lowest-priority line, which it also raises for a
    /// spurious interrupt (one that went away before it was acknowledged).
    MasterSpurious = 7,
    /// The slave's lowest-priority line, which it raises for its spurious ones.
    SlaveSpurious = 15,
}

/// The two registers (I/O ports) of an 8259.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum Register {
    /// Command (ICW1, OCW2, OCW3) and status.
    Command = 0,
    /// Data: the other initialisation words, and the mask of disabled lines.
    Data = 1,
}

/// Commands written to [`Register::Command`].
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Command {
    /// ICW1: start initialisation (bit 4), with an ICW4 to follow (bit 0).
    Initialize = 0x11,
    /// OCW2: non-specific end of interrupt, for the highest line in service.
    EndOfInterrupt = 0x20,
    /// OCW3: make the next read of [`Register::Command`] return the
    /// in-service register (bit 3: OCW3; bit 1: read; bit 0: ISR, not IRR).
    ReadInService = 0x0B,
}

/// ICW4: 8086 mode (not the MCS-80/85 of the original PC design).
const ICW4_8086: u8 = 0x01;
/// A mask that disables every line of a PIC.
const ALL_MASKED: u8 = 0xFF;

/// The task owning each line (0 = unbound).
static OWNER: [AtomicU64; LINES] = [const { AtomicU64::new(0) }; LINES];

/// Short delay between PIC initialisation words (write to an unused port).
fn io_wait() {
    PostCode.write8(0, 0);
}

/// Remap the PICs to vectors 32..47 (by default IRQ 0-7 would alias CPU
/// exceptions 8-15), mask every device line and install the IDT gates.
pub(crate) fn init() {
    let cascade = Line::Cascade as u64;
    for (pic, offset, wiring) in [
        (Pic1, VECTOR_BASE as u8, 1 << cascade),
        (
            Pic2,
            (VECTOR_BASE as u64 + LINES_PER_PIC) as u8,
            cascade as u8,
        ),
    ] {
        pic.write8(Register::Command as u16, Command::Initialize as u8);
        io_wait();
        pic.write8(Register::Data as u16, offset); // ICW2: vector of its line 0.
        io_wait();
        // ICW3: the master gets a bit mask of the lines with a slave on them;
        // the slave gets the number of the master's line it is wired to.
        pic.write8(Register::Data as u16, wiring);
        io_wait();
        pic.write8(Register::Data as u16, ICW4_8086);
        io_wait();
    }
    // Everything masked but the cascade.
    Pic1.write8(Register::Data as u16, ALL_MASKED & !(1u8 << cascade));
    Pic2.write8(Register::Data as u16, ALL_MASKED);
    for (line, &stub) in IRQ_STUBS.iter().enumerate() {
        idt::set_handler(VECTOR_BASE + line, stub_address(stub));
    }
}

/// The PIC that `line` belongs to, and its bit in that PIC's registers.
fn pic_of(line: u64) -> (Device, u8) {
    let pic = if line < LINES_PER_PIC { Pic1 } else { Pic2 };
    (pic, 1 << (line % LINES_PER_PIC))
}

fn set_masked(line: u64, masked: bool) {
    let (pic, bit) = pic_of(line);
    let m = pic.read8(Register::Data as u16);
    pic.write8(
        Register::Data as u16,
        if masked { m | bit } else { m & !bit },
    );
}

/// Whether `line` is really being serviced (according to its PIC's
/// in-service register), rather than a spurious interrupt.
fn in_service(line: Line) -> bool {
    let (pic, bit) = pic_of(line as u64);
    pic.write8(Register::Command as u16, Command::ReadInService as u8);
    pic.read8(Register::Command as u16) & bit != 0
}

/// Acknowledges the interrupt on `pic`.
fn end_of_interrupt(pic: Device) {
    pic.write8(Register::Command as u16, Command::EndOfInterrupt as u8);
}

/// Called (with interrupts disabled) by the IRQ stubs in `arch::entry`.
pub(crate) extern "C" fn irq_dispatch(line: u64) {
    // Spurious interrupts (raised without being in service) must not be
    // acknowledged on their own PIC.
    if line == Line::MasterSpurious as u64 && !in_service(Line::MasterSpurious) {
        return;
    }
    if line == Line::SlaveSpurious as u64 && !in_service(Line::SlaveSpurious) {
        end_of_interrupt(Pic1); // The master did see the cascade.
        return;
    }
    // Keep the line quiet until its driver has serviced the device.
    set_masked(line, true);
    if line >= LINES_PER_PIC {
        end_of_interrupt(Pic2);
    }
    end_of_interrupt(Pic1);
    let owner = OWNER[line as usize].load(Ordering::Relaxed);
    if owner != 0 {
        task::irq_notify(owner, line);
    }
}

/// Returns the task owning `line`, or 0.
pub(crate) fn owner(line: usize) -> u64 {
    OWNER[line].load(Ordering::Relaxed)
}

/// Routes IRQ `line` to the calling, privileged task (the `irq_bind` system call).
pub(crate) fn bind(line: u64) -> i64 {
    if line >= LINES as u64 || line == Line::Cascade as u64 {
        return -EINVAL;
    }
    if !task::current_privileged() {
        return -EPERM;
    }
    let me = task::current_tid();
    let owner = OWNER[line as usize].load(Ordering::Relaxed);
    if owner != 0 && owner != me && task::is_alive(owner) {
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
    if owner(line as usize) != task::current_tid() {
        return -EPERM;
    }
    set_masked(line, false);
    0
}

/// Unbind and mask every line owned by task `tid` (it has ended).
pub(crate) fn release_all(tid: u64) {
    for (line, owner) in OWNER.iter().enumerate() {
        if owner.load(Ordering::Relaxed) == tid {
            owner.store(0, Ordering::Relaxed);
            set_masked(line as u64, true);
        }
    }
}
