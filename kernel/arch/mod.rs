// SPDX-License-Identifier: GPL-2.0

//! Hardware layer: the CPU architecture.
//!
//! One implementation per CPU architecture, in its own directory, selected at compile time; the
//! machine around the CPU is [`crate::platform`]'s business. This module is the boundary: the
//! rest of the kernel uses what is re-exported here, and an architecture implements all of it.
//! The contract is still being carved out of the first implementation (x86_64): for now it is
//! these modules; the [`UserRegs`] trait below is its first compiler-checked piece, and what a
//! new architecture must provide is what the re-exports name.
//!
//! The architecture may use the kernel's generic modules (as `entry` calls the system call
//! dispatcher) and [`crate::platform`] may use the architecture, but nothing here depends on a
//! particular platform, with one exception: the boot entry speaks the protocol of the boot
//! loader that starts the kernel, which is the platform's (`boot` takes the Multiboot handshake
//! from [`crate::platform::multiboot`]).
//!
//! Everything here may use unsafe Rust (see `main.rs`); the module documentation of each part
//! says what makes its interface safe.

#[cfg(target_arch = "x86_64")]
#[path = "x86_64/mod.rs"]
mod imp;

pub(crate) use imp::UserContext;
pub(crate) use imp::{apic, boot, context, cpu, entry, gdt, idt, percpu, phys, smp, upcall, x86};

/// What generic code may know about a thread's saved user registers ([`UserContext`]).
///
/// Everything else about them (how many there are, their names, segment or mode bits) is the
/// architecture's own business.
pub(crate) trait UserRegs: Copy {
    /// Registers for entering a new program at `entry` with its stack pointer at `sp`.
    fn new_program(entry: u64, sp: u64) -> Self;

    /// Whether the registers were saved in user mode (an interrupt can also arrive while the
    /// kernel idles: then there is no thread to act on).
    fn in_user_mode(&self) -> bool;

    /// The number of the system call the thread made.
    fn syscall_nr(&self) -> u64;

    /// The six arguments of the system call.
    fn syscall_args(&self) -> [u64; 6];

    /// Sets what the thread gets back (from a system call, or from the call a forked child
    /// returns from).
    fn set_return(&mut self, value: i64);

    /// What the thread would get back.
    fn returned(&self) -> i64;

    /// Backs up to make system call `nr` again when the thread resumes (`SA_RESTART`).
    fn restart_syscall(&mut self, nr: u64);

    /// The stack pointer.
    fn sp(&self) -> u64;

    /// Moves the stack pointer (a new thread starts on its own stack).
    fn set_sp(&mut self, sp: u64);
}

// The architecture's context must implement the contract.
const _: () = {
    const fn conforms<T: UserRegs>() {}
    conforms::<UserContext>();
};
