// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS kernel.
//!
//! A bare-bones microkernel (for x86_64, so far) that provides mechanisms and leaves policy to
//! user space.
//!
//! The kernel provides physical and virtual memory, tasks with private address spaces and any
//! number of threads, a preemptive scheduler for every CPU of the machine, message-passing IPC
//! and a small Linux-flavoured system call ABI. Policy and drivers
//! live in user-space servers that the boot loader hands over as boot modules (see `user/`):
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

mod device;
mod elf;
mod fs;
mod irq;
mod pmm;
mod program;
mod syscall;
mod task;
mod upcall;
mod vmm;

use arch::{percpu, Cpu, Processor};
use platform::{Board, BootInfo, Machine};

/// Called by the architecture's boot entry once the boot CPU runs Rust code.
pub fn kmain() -> ! {
    serial::init();
    println!(
        "HaisuliOS {} - {} microkernel",
        env!("CARGO_PKG_VERSION"),
        Cpu::NAME
    );
    if !Board::has_memory_map() {
        panic!("the boot loader gave no memory map");
    }
    Cpu::init_cpu(true, Board::wait_us);
    pmm::init();
    irq::init();
    task::cpu_online();
    task::spawn_boot_modules();
    start_cpus();
    println!("[kernel] entering user space");
    task::start()
}

/// Starts the other CPUs that the firmware lists, and reports how many run.
fn start_cpus() {
    let running = Cpu::start_cpus(Board::cpus().as_slice(), Board::wait_us);
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

/// Called by the architecture on every other CPU once it runs Rust code, with the CPU's index:
/// sets the CPU up like the boot CPU, then idles until there are threads to run.
pub fn ap_main(_cpu: usize) -> ! {
    Cpu::init_cpu(false, Board::wait_us);
    task::cpu_online();
    task::idle()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    serial::panic_print(format_args!(
        "\n*** KERNEL PANIC (CPU {}) ***\n{}\n",
        percpu::index(),
        info
    ));
    Board::power_off(1)
}
