// SPDX-License-Identifier: GPL-2.0

//! Hardware layer: the CPU architecture.
//!
//! One implementation per CPU architecture, in its own directory, selected at compile time; the
//! machine around the CPU is [`crate::platform`]'s business. This module is the boundary: the
//! rest of the kernel uses only what is re-exported here, and an architecture implements all of
//! it. Traits make the compiler check the parts of the contract:
//!
//! - [`UserRegs`]: a thread's saved user registers ([`UserContext`]);
//! - [`PageTables`]: the page tables' shape and entry format ([`Paging`]);
//! - [`Processor`]: setting up a CPU, starting the others, preparing a CPU for a thread, idling
//!   ([`Cpu`]);
//! - [`CpuInterrupts`]: the CPU's own interrupts, the scheduler's tick and the reschedule IPI
//!   ([`Cpu`]). The architecture's interrupt entry tells the generic dispatch
//!   ([`crate::irq::dispatch`]) where an interrupt came from, as an [`Interrupt`];
//! - [`IoPermissions`]: the I/O ports a task may use, for architectures that have an I/O port
//!   space (x86's; arm64 has none) ([`IoPorts`]).
//!
//! Besides those, generic code uses the modules re-exported below, which every architecture
//! provides with the same interface: [`context`] (kernel stacks and the context switch),
//! [`percpu`] (which CPU this is), [`phys`] (physical memory access) and [`upcall`] (the frame
//! that enters a handler); and constants such as [`ELF_MACHINE`].
//!
//! The architecture may use the kernel's generic modules (as its entry calls the system call
//! dispatcher), and a platform built on an architecture may use that architecture's own modules
//! by name (the PC's devices are reached with x86's port I/O, through `arch::x86_64`). Nothing
//! here depends on a particular platform, with one exception: the boot entry speaks the protocol
//! of the boot loader that starts the kernel, which is the platform's (x86_64's `boot` takes the
//! Multiboot handshake from `platform::pc`). Generic code never names an architecture's or a
//! platform's directory.
//!
//! Everything here may use unsafe Rust (see `main.rs`); the module documentation of each part
//! says what makes its interface safe.

#[cfg(target_arch = "aarch64")]
pub(crate) mod aarch64;
#[cfg(target_arch = "aarch64")]
use aarch64 as imp;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;
#[cfg(target_arch = "x86_64")]
use x86_64 as imp;

use core::ops::Range;
pub(crate) use imp::{context, percpu, phys, upcall};
pub(crate) use imp::{Cpu, IoPorts, Paging, UserContext, ELF_MACHINE};

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
    /// An entry that maps the device registers at `frame` with `prot`: uncached, and marked so
    /// that [`is_device`](Self::is_device) tells it from a page of memory.
    fn device_entry(frame: u64, prot: Prot) -> u64;
    /// Whether a page entry maps device registers ([`device_entry`](Self::device_entry)).
    fn is_device(entry: u64) -> bool;
    /// What a page entry allows.
    fn page_prot(entry: u64) -> Prot;
    /// The root of the address space with no user pages, for a CPU that runs no thread.
    fn kernel_root() -> u64;
    /// Gives the new, zeroed root table at `root` the kernel's part of every address space.
    fn init_root(root: u64);
    /// Makes `root` this CPU's address space (unless it is already).
    fn activate(root: u64);
}

/// Where an interrupt came from: what the architecture's interrupt entry tells the generic
/// dispatch ([`crate::irq::dispatch`]), whatever the hardware numbers it by (x86 vectors, GIC
/// interrupt ids).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Interrupt {
    /// This CPU's periodic tick (see [`CpuInterrupts::start_tick`]).
    Tick,
    /// Another CPU asks this one to look at the scheduler (see
    /// [`CpuInterrupts::send_reschedule`]).
    Reschedule,
    /// Interrupt line `n` of the platform's interrupt controller (see
    /// [`crate::platform::InterruptController`]).
    Line(usize),
    /// Nothing: the hardware raised an interrupt that needs no handling, and no end of
    /// interrupt either.
    Spurious,
}

/// The CPU: setting each one up, starting the others, and running threads on it.
///
/// A CPU's *index* is the kernel's own number for it ([`percpu::index`]): 0 for the boot CPU,
/// then in the order the others started. The hardware's numbers for CPUs (APIC ids on x86, MPIDR
/// values on arm64) are the architecture's and the platform's business.
pub(crate) trait Processor {
    /// The architecture's name, for the boot banner (`"x86_64"`).
    const NAME: &'static str;

    /// Sets up the CPU this runs on for the kernel: trap and interrupt entry, its own interrupt
    /// controller and tick, system call entry, whatever else it needs. Every CPU calls it once;
    /// the boot CPU (`boot_cpu`) first, before any other is started, and it also sets up what
    /// the CPUs share. `wait(us)` busy-waits for some microseconds, should the CPU need a ruler
    /// (to measure its tick, say).
    fn init_cpu(boot_cpu: bool, wait: fn(u32));

    /// Starts the CPUs with hardware ids `ids` (the boot CPU's among them is skipped), with
    /// `wait(us)` for the delays that starting them takes. Each one started calls
    /// [`crate::ap_main`] with its index. Returns how many CPUs run, the boot CPU included.
    fn start_cpus(ids: &[u64], wait: fn(u32)) -> usize;

    /// Prepares this CPU for running the thread of context slot `slot` (see [`context`]), whose
    /// task has the I/O permissions `io`: whatever the CPU must know before it switches to the
    /// thread, or after the permissions change, such as where the thread's kernel stack is.
    fn prepare_thread(slot: usize, io: &IoPorts);

    /// Enables interrupts, sleeps until one has been handled, and disables them again (an
    /// interrupt that arrives in between is not missed).
    fn wait_for_interrupt();

    /// Stops this CPU for good.
    fn halt() -> !;
}

/// The CPU's own interrupts: the scheduler's tick, and the interrupt with which one CPU makes
/// another look at the scheduler.
///
/// The platform's interrupt lines are [`crate::platform::InterruptController`]'s business.
pub(crate) trait CpuInterrupts {
    /// Starts this CPU's periodic tick ([`Interrupt::Tick`]), which arrives once interrupts are
    /// enabled.
    fn start_tick();

    /// Interrupts CPU `cpu` (an index) with [`Interrupt::Reschedule`].
    fn send_reschedule(cpu: usize);

    /// Acknowledges the CPU's own interrupt being handled (a tick or a reschedule), so that the
    /// next one can come.
    fn end_of_interrupt();
}

/// The I/O ports a task may use: the part of device grants that belongs to an architecture with
/// an I/O port space (x86's). Opaque to generic code, which keeps one per task and hands it to
/// [`Processor::prepare_thread`]; an architecture without ports allows none.
pub(crate) trait IoPermissions: Copy {
    /// No ports: what every task starts with.
    const NONE: Self;

    /// Lets the task use the ports in `ports`; `false` (and no change) if the architecture has
    /// no such ports.
    fn allow(&mut self, ports: Range<u64>) -> bool;
}

// The architecture's types must implement the contract.
const _: () = {
    const fn regs<T: UserRegs>() {}
    const fn tables<T: PageTables>() {}
    const fn cpu<T: Processor + CpuInterrupts>() {}
    const fn ports<T: IoPermissions>() {}
    regs::<UserContext>();
    tables::<Paging>();
    cpu::<Cpu>();
    ports::<IoPorts>();
};
