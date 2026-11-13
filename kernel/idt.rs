// SPDX-License-Identifier: GPL-2.0

//! Interrupt descriptor table.
//!
//! Installs the IDT and handles CPU exceptions (vectors 0 to 31).
//!
//! Hardware interrupts (vectors 32 to 47) are installed by [`crate::irq::init`].

use crate::arch::cpu;
use crate::arch::entry::{stub_address, ExceptionFrame, EXCEPTION_STUBS};
use crate::arch::x86::{gate_descriptor, Exception, GateType, PrivilegeLevel};
use crate::gdt::{DOUBLE_FAULT_IST, KERNEL_CS};
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::signal::SIGSEGV;
use haisuli_abi::Termination;

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

pub(crate) fn init() {
    for (vector, &stub) in EXCEPTION_STUBS.iter().enumerate() {
        let ist = if vector == Exception::DoubleFault as usize {
            DOUBLE_FAULT_IST
        } else {
            NO_IST
        };
        set_gate(vector, stub_address(stub), ist);
    }
    cpu::load_idt(&IDT);
}

/// Install the gate for a hardware interrupt vector.
pub(crate) fn set_handler(vector: usize, handler: u64) {
    set_gate(vector, handler, 0);
}

/// Called by the exception stubs in `arch::entry`.
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
        // Behave like POSIX SIGSEGV: terminate the process, wait status = 11.
        crate::process::exit_current(Termination::Signalled(SIGSEGV).wait_status())
    }
    panic!("unrecoverable exception in kernel mode");
}
