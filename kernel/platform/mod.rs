// SPDX-License-Identifier: GPL-2.0

//! Hardware layer: the platform.
//!
//! The machine around the CPU: how the kernel is booted and what the loader left, the interrupt
//! controller, the serial line, the timer that measures boot-time delays, how the other CPUs
//! are found, and how the machine powers off. One implementation per platform; there is one per
//! architecture for now, so the architecture selects it (a feature could choose among boards
//! later).
//!
//! The platform may use [`crate::arch`] (its devices are reached with the CPU's instructions);
//! the architecture does not depend on the platform, except that the boot entry speaks the boot
//! loader's protocol (see [`crate::arch`]).
//!
//! Everything here may use unsafe Rust (see `main.rs`); the module documentation of each part
//! says what makes its interface safe.

#[cfg(target_arch = "x86_64")]
#[path = "pc/mod.rs"]
mod imp;

pub(crate) use imp::{acpi, multiboot, pic, pit, power, uart};
