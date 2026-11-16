// SPDX-License-Identifier: GPL-2.0

//! Interrupt descriptor table.
//!
//! Installs the IDT: the gates of the CPU exceptions (vectors 0 to 31), of the platform's
//! interrupt lines (from [`IRQ_VECTOR_BASE`] on) and of the local APIC's vectors; handles the
//! exceptions, and tells the generic interrupt dispatch where an interrupt came from (an
//! [`Interrupt`], not a vector).

use super::apic::{RESCHEDULE_VECTOR, SPURIOUS_VECTOR, TIMER_VECTOR};
use super::cpu;
use super::entry::{
    stub_address, ExceptionFrame, TrapFrame, APIC_STUBS, EXCEPTION_STUBS, IRQ_STUBS,
};
use super::gdt::{DOUBLE_FAULT_IST, KERNEL_CS};
use super::x86::{gate_descriptor, Exception, GateType, PrivilegeLevel};
use crate::arch::Interrupt;
use crate::task::{self, Why};
use core::sync::atomic::{AtomicU64, Ordering};

/// The first vector after the CPU's own: where the platform's interrupt lines start.
pub(crate) const IRQ_VECTOR_BASE: usize = Exception::VECTORS;
/// The vectors of the platform's interrupt lines: one entry stub each.
const IRQ_VECTORS: core::ops::Range<u64> =
    IRQ_VECTOR_BASE as u64..(IRQ_VECTOR_BASE + IRQ_STUBS.len()) as u64;

/// Vectors in the IDT: every vector there is.
const GATES: usize = 256;
/// Each gate is 16 bytes: two words.
static IDT: [AtomicU64; 2 * GATES] = [const { AtomicU64::new(0) }; 2 * GATES];

/// Interrupt-stack-table slot 0: no stack switch (the current stack, or the
/// kernel stack on entry from user mode).
const NO_IST: u8 = 0;

/// Installs a kernel-only interrupt gate: user code cannot `int` into it.
fn set_gate(vector: usize, handler: u64, ist: u8) {
    let [low, high] = gate_descriptor(
        handler,
        KERNEL_CS,
        ist,
        GateType::Interrupt,
        PrivilegeLevel::Kernel,
    );
    IDT[2 * vector].store(low, Ordering::Relaxed);
    IDT[2 * vector + 1].store(high, Ordering::Relaxed);
}

/// Fills in every gate and loads the IDT on the boot CPU.
pub(crate) fn init() {
    for (vector, &stub) in EXCEPTION_STUBS.iter().enumerate() {
        let ist = if vector == Exception::DoubleFault as usize {
            DOUBLE_FAULT_IST
        } else {
            NO_IST
        };
        set_gate(vector, stub_address(stub), ist);
    }
    for (line, &stub) in IRQ_STUBS.iter().enumerate() {
        set_gate(IRQ_VECTOR_BASE + line, stub_address(stub), NO_IST);
    }
    for (vector, &stub) in [TIMER_VECTOR, RESCHEDULE_VECTOR, SPURIOUS_VECTOR]
        .iter()
        .zip(APIC_STUBS.iter())
    {
        set_gate(*vector as usize, stub_address(stub), NO_IST);
    }
    load();
}

/// Loads the IDT (shared by every CPU) on this CPU.
pub(crate) fn load() {
    cpu::load_idt(&IDT);
}

/// Where the interrupt with `vector` came from.
fn source(vector: u64) -> Interrupt {
    match vector {
        v if IRQ_VECTORS.contains(&v) => Interrupt::Line((v - IRQ_VECTORS.start) as usize),
        v if v == TIMER_VECTOR as u64 => Interrupt::Tick,
        v if v == RESCHEDULE_VECTOR as u64 => Interrupt::Reschedule,
        // The APIC's spurious vector needs no end of interrupt; no other vector has a gate.
        _ => Interrupt::Spurious,
    }
}

/// Called (with interrupts disabled) by the interrupt stubs in [`super::entry`], with the vector
/// and the interrupted registers.
pub(crate) extern "C" fn interrupt_handler(vector: u64, f: &mut TrapFrame) {
    crate::irq::dispatch(source(vector), f);
}

/// Called by the exception stubs in [`super::entry`].
pub(crate) extern "C" fn exception_handler(f: &ExceptionFrame) -> ! {
    let exception = Exception::from_vector(f.vector);
    let from_user = PrivilegeLevel::of_selector(f.cs) == PrivilegeLevel::User;
    crate::println!(
        "\n[{}] #{} ({}) err={:#x} rip={:#x} rsp={:#x}",
        if from_user { "user" } else { "KERNEL" },
        f.vector,
        exception.map_or("exception", Exception::name),
        f.error,
        f.rip,
        f.rsp
    );
    if exception == Some(Exception::PageFault) {
        crate::println!("    faulting address (CR2) = {:#x}", cpu::fault_address());
    }
    if from_user {
        // The task cannot go on. What that means for its process (SIGSEGV) is
        // the task manager's business, which `end_current` tells.
        task::end_current(Why::Fault)
    }
    panic!("unrecoverable exception in kernel mode");
}
