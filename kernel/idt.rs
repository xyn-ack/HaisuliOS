// SPDX-License-Identifier: GPL-2.0

//! Interrupt descriptor table.
//!
//! Installs the IDT and handles CPU exceptions (vectors 0 to 31).
//!
//! Hardware interrupts (vectors 32 to 47) are installed by [`crate::irq::init`].

use crate::arch::cpu;
use crate::arch::entry::{stub_address, ExceptionFrame, EXCEPTION_STUBS};
use crate::gdt::KERNEL_CS;
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::signal::SIGSEGV;
use haisuli_abi::wait_status_signalled;

/// 256 gates of 16 bytes (two words each).
static IDT: [AtomicU64; 512] = [const { AtomicU64::new(0) }; 512];

/// Vector of the double fault, which runs on its own stack (IST 1).
const DOUBLE_FAULT: usize = 8;

/// Install a kernel-only interrupt gate (`ist` 0 = current stack).
fn set_gate(vector: usize, handler: u64, ist: u64) {
    let low = (handler & 0xFFFF)
        | (KERNEL_CS << 16)
        | (ist << 32)
        | (0x8E << 40) // Present, DPL0, interrupt gate.
        | ((handler >> 16) & 0xFFFF) << 48;
    IDT[2 * vector].store(low, Ordering::Relaxed);
    IDT[2 * vector + 1].store(handler >> 32, Ordering::Relaxed);
}

pub(crate) fn init() {
    for (vector, &stub) in EXCEPTION_STUBS.iter().enumerate() {
        set_gate(
            vector,
            stub_address(stub),
            if vector == DOUBLE_FAULT { 1 } else { 0 },
        );
    }
    cpu::load_idt(&IDT);
}

/// Install the gate for a hardware interrupt vector.
pub(crate) fn set_handler(vector: usize, handler: u64) {
    set_gate(vector, handler, 0);
}

fn name(v: u64) -> &'static str {
    match v {
        0 => "divide error",
        6 => "invalid opcode",
        8 => "double fault",
        13 => "general protection fault",
        14 => "page fault",
        _ => "exception",
    }
}

/// Called by the exception stubs in `arch::entry`.
pub(crate) extern "C" fn exception_handler(f: &ExceptionFrame) -> ! {
    let from_user = f.cs & 3 == 3;
    crate::println!(
        "\n[{}] #{} ({}) err={:#x} rip={:#x} rsp={:#x}",
        if from_user { "user" } else { "KERNEL" },
        f.vector,
        name(f.vector),
        f.error,
        f.rip,
        f.rsp
    );
    if f.vector == 14 {
        crate::println!("    faulting address (CR2) = {:#x}", cpu::fault_address());
    }
    if from_user {
        // Behave like POSIX SIGSEGV: terminate the process, wait status = 11.
        crate::process::exit_current(wait_status_signalled(SIGSEGV))
    }
    panic!("unrecoverable exception in kernel mode");
}
