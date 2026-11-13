// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS user-space runtime.
//!
//! Everything a HaisuliOS program needs to start, make system calls, print and panic.
//!
//! It provides the program entry point ([`entry!`]), system calls ([`sys`]), buffered console
//! output ([`print!`] / [`println!`]) and a panic handler. Programs are `#![no_std]`, `#![no_main]`
//! static executables linked at `USER_BASE` by `user/user.ld`.

#![no_std]

pub use haisuli_abi as abi;

pub mod io;
pub mod sys;

/// What a new program is started with.
///
/// Nothing yet: the boot ramdisk, which init used to get here, is now in the
/// kernel's file system. It is the place for arguments once there is `exec`.
#[non_exhaustive]
pub struct StartInfo {}

/// Define the program's entry point: `entry!(main)` with
/// `fn main(info: StartInfo) -> i32`; the return value is the exit status.
///
/// The expansion contains no unsafe blocks, only the `_start` assembly: it
/// sets up a proper SysV call frame (the kernel starts us with an aligned
/// stack and no return address) and passes the address of `main` to
/// `__start`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        // Compile-time check that `main` has the signature `__start` expects.
        const _: fn($crate::StartInfo) -> i32 = $main;

        core::arch::global_asm!(
            ".pushsection .text._start, \"ax\"",
            ".global _start",
            "_start:",
            "    xor ebp, ebp",
            "    and rsp, -16",
            "    lea rdi, [rip + {main}]",
            "    call {start}",
            "    ud2",
            ".popsection",
            main = sym $main,
            start = sym $crate::__start,
        );
    };
}

/// Runtime entry, called only by the `_start` that `entry!` defines.
#[doc(hidden)]
// `main` is a Rust function called by Rust code below; the assembly only
// passes its address through (a pointer-sized, non-null value), and `entry!`
// checks its type at compile time.
#[expect(improper_ctypes_definitions)]
pub extern "C" fn __start(main: fn(StartInfo) -> i32) -> ! {
    sys::exit(main(StartInfo {}))
}

/// Panics are reported on the kernel debug log, which works even when the
/// console server is unavailable (or is the process that panicked).
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    use core::fmt::Write;
    let mut w = io::BufWriter::new(io::Sink::DebugLog);
    let _ = writeln!(w, "[pid {}] panic: {}", sys::getpid(), info);
    w.flush();
    sys::exit(101)
}
