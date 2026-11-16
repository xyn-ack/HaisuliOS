// SPDX-License-Identifier: GPL-2.0

//! The x86_64 architecture.
//!
//! What [`crate::arch`] re-exports, for x86_64: long mode, the `syscall` instruction, 4-level
//! page tables, the local APIC and the INIT / start-up IPI sequence.

pub(crate) mod apic;
pub(crate) mod boot;
pub(crate) mod context;
pub(crate) mod cpu;
pub(crate) mod entry;
pub(crate) mod gdt;
pub(crate) mod idt;
pub(crate) mod percpu;
pub(crate) mod phys;
pub(crate) mod smp;
pub(crate) mod upcall;
pub(crate) mod x86;

pub(crate) use entry::TrapFrame as UserContext;
