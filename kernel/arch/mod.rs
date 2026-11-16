// SPDX-License-Identifier: GPL-2.0

//! Hardware layer.
//!
//! The only part of the kernel, besides [`crate::sync::Lock`], that is allowed to use unsafe Rust.
//!
//! Everything else is checked safe code (`#![deny(unsafe_code)]` in `main.rs`). Each module wraps
//! one kind of low-level access behind a safe interface whose restrictions are what make it safe:
//!
//! - [`boot`]: Multiboot entry, 32-bit to 64-bit switch, boot page tables.
//! - [`cpu`]: privileged instructions, port I/O on a fixed list of devices.
//! - [`percpu`]: each CPU's own data, reached through GS.
//! - [`apic`]: the local APICs: the scheduler's tick, IPIs; the PIT as a stopwatch.
//! - [`acpi`]: finding the CPUs in the firmware's ACPI tables.
//! - [`smp`]: starting the other CPUs.
//! - [`entry`]: exception, IRQ and system call entry stubs, return to user mode.
//! - [`context`]: kernel stacks and context switching.
//! - [`phys`]: physical memory, limited to allocator-owned frames.
//! - [`multiboot`]: the boot loader's (reserved) information and modules.
//! - [`x86`]: names for the CPU's own numbers (register bits, vectors, descriptor formats); no
//!   code that touches the hardware.

pub(crate) mod acpi;
pub(crate) mod apic;
pub(crate) mod boot;
pub(crate) mod context;
pub(crate) mod cpu;
pub(crate) mod entry;
pub(crate) mod multiboot;
pub(crate) mod percpu;
pub(crate) mod phys;
pub(crate) mod smp;
pub(crate) mod x86;
