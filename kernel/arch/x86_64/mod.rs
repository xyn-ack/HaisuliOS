// SPDX-License-Identifier: GPL-2.0

//! The x86_64 architecture.
//!
//! What [`crate::arch`] re-exports, for x86_64: long mode, the `syscall` instruction, 4-level
//! page tables, the local APIC and the INIT / start-up IPI sequence. [`Cpu`] implements the
//! contract's [`Processor`] and [`CpuInterrupts`] with the modules below; generic code never
//! names them.
//!
//! The modules are visible to the platforms built on x86_64 (the PC reaches its devices with
//! [`cpu::Device`]'s port I/O, and its 8259s deliver at [`idt::IRQ_VECTOR_BASE`]).

pub(crate) mod apic;
pub(crate) mod boot;
pub(crate) mod context;
pub(crate) mod cpu;
pub(crate) mod entry;
pub(crate) mod gdt;
pub(crate) mod idt;
pub(crate) mod paging;
pub(crate) mod percpu;
pub(crate) mod phys;
pub(crate) mod smp;
pub(crate) mod upcall;
pub(crate) mod x86;

use crate::arch::{CpuInterrupts, Processor};

pub(crate) use entry::TrapFrame as UserContext;
pub(crate) use gdt::IoPorts;
pub(crate) use paging::Paging;

/// `e_machine` of the programs this CPU runs: `EM_X86_64`.
pub(crate) const ELF_MACHINE: u16 = 62;

/// An x86_64 CPU, with its local APIC.
pub(crate) struct Cpu;

impl Processor for Cpu {
    const NAME: &'static str = "x86_64";

    /// Loads the GDT and this CPU's TSS, the IDT (the boot CPU fills it in first: every CPU
    /// shares it), enables the local APIC (the boot CPU's passes the PICs' interrupts through,
    /// and measures the APIC timer with `wait`), and the `syscall` instruction.
    fn init_cpu(boot_cpu: bool, wait: fn(u32)) {
        gdt::init();
        if boot_cpu {
            idt::init();
        } else {
            idt::load();
        }
        apic::init(boot_cpu);
        if boot_cpu {
            apic::calibrate_timer(wait);
        }
        entry::init_syscalls();
    }

    /// Wakes the CPUs with the given APIC ids through the real-mode trampoline.
    fn start_cpus(ids: &[u64], wait: fn(u32)) -> usize {
        smp::start_cpus(ids, wait)
    }

    /// The thread's kernel stack, for interrupts (the TSS) and for `syscall` (the per-CPU
    /// block), and its task's I/O bitmap (the TSS).
    fn prepare_thread(slot: usize, io: &IoPorts) {
        let top = context::stack_top(slot);
        gdt::set_thread(top, io);
        percpu::set_syscall_stack(top);
    }

    fn wait_for_interrupt() {
        cpu::wait_for_interrupt();
    }

    fn halt() -> ! {
        cpu::halt()
    }
}

/// The local APIC's timer and inter-processor interrupts.
impl CpuInterrupts for Cpu {
    const TICK_MS: u64 = apic::TICK_MS as u64;

    fn start_tick() {
        apic::start_timer();
    }

    fn send_reschedule(cpu: usize) {
        apic::send_reschedule(cpu);
    }

    fn end_of_interrupt() {
        apic::end_of_interrupt();
    }
}
