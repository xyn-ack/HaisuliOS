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

use core::ops::Range;
pub(crate) use imp::{apic, context, cpu, entry, gdt, idt, percpu, phys, smp, upcall, x86};
pub(crate) use imp::{Paging, UserContext, ELF_MACHINE};

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

/// What a user page may be used for.
///
/// Readable always; the page-table format encodes the rest its own way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Prot {
    /// User mode may access it (otherwise only the kernel).
    pub(crate) user: bool,
    /// It may be written.
    pub(crate) write: bool,
    /// Instructions may be fetched from it.
    pub(crate) exec: bool,
}

/// The architecture's page tables: their shape, and the format of their entries.
///
/// [`crate::vmm`] walks them generically: a root table, then `LEVELS - 1` levels of tables
/// below it, each translating `INDEX_BITS` bits of the virtual address above a page offset of
/// `PAGE_SHIFT` bits, with 8-byte entries; the last level's entries map pages. Everything about
/// the bits of an entry, and about how the kernel's half of an address space gets into a new one
/// (a shared root entry on x86_64, a root register of its own on arm64), is the architecture's.
pub(crate) trait PageTables {
    /// Levels of tables, the root's included.
    const LEVELS: usize;
    /// Virtual address bits that each level translates (a table has `2^INDEX_BITS` entries).
    const INDEX_BITS: u32;
    /// Bits of the page offset: a page is `2^PAGE_SHIFT` bytes.
    const PAGE_SHIFT: u32;
    /// The root entries that map user space (and nothing else).
    const USER_ROOT_ENTRIES: Range<usize>;

    /// Whether `entry` is in use.
    fn is_present(entry: u64) -> bool;
    /// The physical address of the table or page that `entry` points to.
    fn target(entry: u64) -> u64;
    /// An entry that points to the next level's table at `table`.
    fn table_entry(table: u64) -> u64;
    /// An entry that maps the page at `frame` with `prot`.
    fn page_entry(frame: u64, prot: Prot) -> u64;
    /// What a page entry allows.
    fn page_prot(entry: u64) -> Prot;
    /// The root of the address space with no user pages, for a CPU that runs no thread.
    fn kernel_root() -> u64;
    /// Gives the new, zeroed root table at `root` the kernel's part of every address space.
    fn init_root(root: u64);
    /// Makes `root` this CPU's address space (unless it is already).
    fn activate(root: u64);
}

// The architecture's types must implement the contract.
const _: () = {
    const fn regs<T: UserRegs>() {}
    const fn tables<T: PageTables>() {}
    regs::<UserContext>();
    tables::<Paging>();
};
