// SPDX-License-Identifier: GPL-2.0

//! The arm64 (AArch64) architecture.
//!
//! What [`crate::arch`] re-exports, for arm64: the kernel at EL1 and user space at EL0, the `svc`
//! instruction, 4-level page tables with user space in `TTBR0_EL1` and the kernel in
//! `TTBR1_EL1`, the GICv3 CPU interface (its system registers) and the generic timer. [`Cpu`]
//! implements the contract's [`Processor`] and [`CpuInterrupts`] with the modules below; generic
//! code never names them. The modules are visible to the platforms built on arm64 (QEMU's
//! `virt` machine reaches its devices through the kernel's linear map, [`phys::virt`]).
//!
//! The kernel runs in the upper half of the address space: its image, and all of physical memory
//! (the *linear map*), from [`KERNEL_OFFSET`] on. User space has the lower half to itself, one
//! root table per address space.

pub(crate) mod boot;
pub(crate) mod context;
pub(crate) mod cpu;
pub(crate) mod entry;
pub(crate) mod gic;
pub(crate) mod paging;
pub(crate) mod percpu;
pub(crate) mod phys;
pub(crate) mod psci;
pub(crate) mod smp;
pub(crate) mod timer;
pub(crate) mod upcall;

use crate::arch::{CpuInterrupts, IoPermissions, Processor};
use core::ops::Range;

pub(crate) use entry::TrapFrame as UserContext;
pub(crate) use paging::Paging;

/// `e_machine` of the programs this CPU runs: `EM_AARCH64`.
pub(crate) const ELF_MACHINE: u16 = 183;

/// Where the kernel's half of the address space starts: virtual address `KERNEL_OFFSET + p` is
/// physical address `p` (`TTBR1_EL1` translates the top 48 bits' worth of addresses).
pub(crate) const KERNEL_OFFSET: u64 = 0xFFFF_0000_0000_0000;

/// An arm64 CPU, with its GIC CPU interface and generic timer.
pub(crate) struct Cpu;

impl Processor for Cpu {
    const NAME: &'static str = "aarch64";

    /// The boot entry has enabled the MMU, FP/SIMD and the exception vectors; this drops the
    /// boot identity map from `TTBR0_EL1`, enables the GIC CPU interface and records the CPU's
    /// affinity, for the reschedule IPI. The generic timer needs no measuring: it tells its
    /// frequency.
    fn init_cpu(_boot_cpu: bool, _wait: fn(u32)) {
        paging::drop_identity_map();
        gic::init_cpu();
        timer::stop();
    }

    /// Starts the CPUs with the given MPIDR affinities through PSCI `CPU_ON`.
    fn start_cpus(ids: &[u64], wait: fn(u32)) -> usize {
        smp::start_cpus(ids, wait)
    }

    /// Nothing: an exception from EL0 uses the stack pointer that the return to EL0 left, the
    /// top of the thread's kernel stack (see [`entry`]).
    fn prepare_thread(_slot: usize, _io: &IoPorts) {}

    fn wait_for_interrupt() {
        cpu::wait_for_interrupt();
    }

    fn halt() -> ! {
        cpu::halt()
    }
}

/// The GIC CPU interface and the generic timer's virtual timer.
impl CpuInterrupts for Cpu {
    fn start_tick() {
        timer::start();
    }

    fn send_reschedule(cpu: usize) {
        gic::send_reschedule(cpu);
    }

    fn end_of_interrupt() {
        gic::end_of_interrupt(percpu::take_interrupt());
    }
}

/// arm64 has no I/O port space: a task never has any ports.
#[derive(Clone, Copy)]
pub(crate) struct IoPorts;

impl IoPermissions for IoPorts {
    const NONE: IoPorts = IoPorts;

    fn allow(&mut self, _ports: Range<u64>) -> bool {
        false
    }
}
