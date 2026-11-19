// SPDX-License-Identifier: MIT

//! The interrupt controller.
//!
//! The PC's two cascaded 8259 PICs. They deliver to the boot CPU only (through its local APIC's
//! LINT0 in `ExtINT` mode, the PC's "virtual wire"); [`crate::irq`] forwards the lines to
//! user-space drivers. Their vectors start at [`IRQ_VECTOR_BASE`], where x86_64's
//! [`idt`](crate::arch::x86_64::idt) installs the entry stubs.

use crate::arch::x86_64::cpu::Device::{self, Pic1, Pic2, PostCode};
use crate::arch::x86_64::idt::IRQ_VECTOR_BASE;

/// Lines of each 8259.
const LINES_PER_PIC: usize = 8;
/// Lines of the master and slave PIC together: the interrupt lines the platform has.
pub(crate) const LINES: usize = 2 * LINES_PER_PIC;

/// IRQ lines with a fixed role in the master/slave pair of 8259s.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum Line {
    /// The master's line 2, which the slave is wired to: never a device.
    Cascade = 2,
    /// The master's lowest-priority line, which it also raises for a
    /// spurious interrupt (one that went away before it was acknowledged).
    MasterSpurious = 7,
    /// The slave's lowest-priority line, which it raises for its spurious ones.
    SlaveSpurious = 15,
}

/// The line that is never a device's (the cascade).
pub(crate) const RESERVED_LINE: usize = Line::Cascade as usize;

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

/// Short delay between PIC initialisation words (write to an unused port).
fn io_wait() {
    PostCode.write8(0, 0);
}

/// Remap the PICs to the vectors from [`IRQ_VECTOR_BASE`] on (by default IRQ 0-7 would alias
/// CPU exceptions 8-15) and mask every device line.
pub(crate) fn init() {
    let cascade = Line::Cascade as usize;
    for (pic, offset, wiring) in [
        (Pic1, IRQ_VECTOR_BASE as u8, 1 << cascade),
        (Pic2, (IRQ_VECTOR_BASE + LINES_PER_PIC) as u8, cascade as u8),
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
}

/// The PIC that `line` belongs to, and its bit in that PIC's registers.
fn pic_of(line: usize) -> (Device, u8) {
    let pic = if line < LINES_PER_PIC { Pic1 } else { Pic2 };
    (pic, 1 << (line % LINES_PER_PIC))
}

fn set_masked(line: usize, masked: bool) {
    let (pic, bit) = pic_of(line);
    let m = pic.read8(Register::Data as u16);
    pic.write8(
        Register::Data as u16,
        if masked { m | bit } else { m & !bit },
    );
}

/// Keeps `line` quiet until [`unmask`].
pub(crate) fn mask(line: usize) {
    set_masked(line, true);
}

/// Lets `line` interrupt again.
pub(crate) fn unmask(line: usize) {
    set_masked(line, false);
}

/// Whether `line` is really being serviced (according to its PIC's
/// in-service register), rather than a spurious interrupt.
fn in_service(line: Line) -> bool {
    let (pic, bit) = pic_of(line as usize);
    pic.write8(Register::Command as u16, Command::ReadInService as u8);
    pic.read8(Register::Command as u16) & bit != 0
}

/// Acknowledges the interrupt on `pic`.
fn end_of_interrupt(pic: Device) {
    pic.write8(Register::Command as u16, Command::EndOfInterrupt as u8);
}

/// Takes the interrupt on `line`: masks the line and acknowledges the controller.
///
/// Returns `false` for a spurious interrupt (the line has nothing to deliver).
pub(crate) fn acknowledge(line: usize) -> bool {
    // Spurious interrupts (raised without being in service) must not be
    // acknowledged on their own PIC.
    if line == Line::MasterSpurious as usize && !in_service(Line::MasterSpurious) {
        return false;
    }
    if line == Line::SlaveSpurious as usize && !in_service(Line::SlaveSpurious) {
        end_of_interrupt(Pic1); // The master did see the cascade.
        return false;
    }
    // Keep the line quiet until its driver has serviced the device.
    mask(line);
    if line >= LINES_PER_PIC {
        end_of_interrupt(Pic2);
    }
    end_of_interrupt(Pic1);
    true
}
