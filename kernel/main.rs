// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS kernel.
//!
//! A bare-bones x86_64 microkernel that provides mechanisms and leaves policy to user space.
//!
//! The kernel provides physical and virtual memory, tasks with private address spaces and any
//! number of threads, a preemptive scheduler for every CPU of the machine, message-passing IPC
//! and a small Linux-flavoured system call ABI. Policy and drivers
//! live in user-space servers that the boot loader hands over as Multiboot modules (see `user/`):
//! POSIX processes, for one, are implemented by the process server on top of the kernel's tasks.
//!
//! Unsafe code is confined to the hardware layer, [`arch`] (the CPU architecture) and
//! [`platform`] (the machine around it), and to [`sync`] (the kernel's lock); the compiler
//! rejects it anywhere else.

#![no_std]
#![no_main]
#![deny(unsafe_code)]

#[macro_use]
mod serial;
#[expect(unsafe_code)]
mod arch;
#[expect(unsafe_code)]
mod platform;
#[expect(unsafe_code)]
mod sync;

mod elf;
mod fs;
mod irq;
mod pmm;
mod program;
mod syscall;
mod task;
mod upcall;
mod vmm;

use arch::{apic, cpu, gdt, idt, smp};
use platform::{acpi, multiboot, pit, power};

/// Called by `arch::boot` once the boot CPU is in 64-bit mode.
pub fn kmain() -> ! {
    serial::init();
    println!(
        "HaisuliOS {} - x86_64 microkernel",
        env!("CARGO_PKG_VERSION")
    );
    if !multiboot::has_memory_map() {
        panic!("not started by a Multiboot loader with a memory map");
    }
    gdt::init();
    idt::init();
    pmm::init();
    irq::init();
    apic::init(true);
    apic::calibrate_timer(pit::wait_us);
    syscall::init();
    task::cpu_online(apic::id());
    task::spawn_boot_modules();
    start_cpus();
    println!("[kernel] entering user space");
    task::start()
}

/// Starts the other CPUs that the firmware lists, and reports how many run.
fn start_cpus() {
    let (ids, n) = acpi::cpus();
    let running = smp::start_cpus(&ids[..n], pit::wait_us);
    // Each one says it is online (`ap_main`) right after it starts.
    while task::cpus_online() < running {
        core::hint::spin_loop();
    }
    println!(
        "[kernel] {} CPU{} online",
        running,
        if running == 1 { "" } else { "s" }
    );
}

/// Called by `arch::smp` on every other CPU, once it is in 64-bit mode: sets the CPU up like the
/// boot CPU, then idles until there are threads to run.
pub fn ap_main(_cpu: usize) -> ! {
    gdt::init();
    idt::load();
    apic::init(false);
    syscall::init();
    task::cpu_online(apic::id());
    task::idle()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    serial::panic_print(format_args!(
        "\n*** KERNEL PANIC (CPU {}) ***\n{}\n",
        arch::percpu::index(),
        info
    ));
    power::off(1);
    cpu::halt()
}
