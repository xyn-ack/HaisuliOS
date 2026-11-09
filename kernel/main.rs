// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS kernel.
//!
//! A bare-bones x86_64 microkernel that provides mechanisms and leaves policy to user space.
//!
//! The kernel provides physical and virtual memory, processes with private address spaces, a
//! scheduler, message-passing IPC and a small POSIX-flavoured system call ABI. Policy and drivers
//! live in user-space servers that the boot loader hands over as Multiboot modules (see `user/`).
//!
//! Unsafe code is confined to [`arch`] (the hardware layer) and [`sync`] (the kernel's lock); the
//! compiler rejects it anywhere else.

#![no_std]
#![no_main]
#![deny(unsafe_code)]

#[macro_use]
mod serial;
#[expect(unsafe_code)]
mod arch;
#[expect(unsafe_code)]
mod sync;

mod elf;
mod gdt;
mod idt;
mod irq;
mod pmm;
mod process;
mod syscall;
mod vmm;

use arch::{cpu, multiboot};

/// Called by `arch::boot` once the CPU is in 64-bit mode.
pub fn kmain() -> ! {
    serial::init();
    println!("HaisuliOS 0.1.0 - x86_64 microkernel");
    if !multiboot::has_memory_map() {
        panic!("not started by a Multiboot loader with a memory map");
    }
    gdt::init();
    idt::init();
    pmm::init();
    irq::init();
    syscall::init();
    process::spawn_boot_modules();
    println!("[kernel] entering user space");
    process::start()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    println!("\n*** KERNEL PANIC ***\n{}", info);
    cpu::qemu_exit(1);
    cpu::halt()
}
