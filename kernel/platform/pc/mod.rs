// SPDX-License-Identifier: GPL-2.0

//! The PC platform.
//!
//! What [`crate::platform`] re-exports, for the PC (and QEMU's emulation of it): Multiboot and
//! the ACPI tables, the 8259 PICs, the COM1 UART, the 8254 timer and `isa-debug-exit`.

pub(crate) mod acpi;
pub(crate) mod multiboot;
pub(crate) mod pic;
pub(crate) mod pit;
pub(crate) mod power;
pub(crate) mod uart;
