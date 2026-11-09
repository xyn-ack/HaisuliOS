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
//! - [`entry`]: exception, IRQ and system call entry stubs, return to user mode.
//! - [`context`]: kernel stacks and context switching.
//! - [`phys`]: physical memory, limited to allocator-owned frames.
//! - [`multiboot`]: the boot loader's (reserved) information and modules.

pub(crate) mod boot;
pub(crate) mod context;
pub(crate) mod cpu;
pub(crate) mod entry;
pub(crate) mod multiboot;
pub(crate) mod phys;
