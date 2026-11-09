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

/// What the kernel hands a new boot process (in `rdi` / `rsi`).
pub struct StartInfo {
    /// The boot ramdisk (a ustar archive), mapped read-only.
    ///
    /// Only init gets one; it is empty for every other process.
    pub ramdisk: &'static [u8],
}

/// Define the program's entry point: `entry!(main)` with
/// `fn main(info: StartInfo) -> i32`; the return value is the exit status.
///
/// The expansion contains no unsafe blocks, only the `_start` assembly: it
/// sets up a proper SysV call frame (the kernel starts us with an aligned
/// stack and no return address) and passes the kernel's registers and the
/// address of `main` to `__start`.
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
            "    lea rdx, [rip + {main}]",
            "    call {start}",
            "    ud2",
            ".popsection",
            main = sym $main,
            start = sym $crate::__start,
        );
    };
}

/// Runtime entry, called only by the `_start` that `entry!` defines.
///
/// # Safety
/// `base`/`len` must be the ramdisk registers the kernel passed to the new
/// process (0/0 or memory mapped for the whole program run), and `main` must
/// be a function of the stated type.
#[doc(hidden)]
// `main` is a Rust function called by Rust code below; the assembly only
// passes its address through (a pointer-sized, non-null value).
#[expect(improper_ctypes_definitions)]
pub unsafe extern "C" fn __start(base: u64, len: u64, main: fn(StartInfo) -> i32) -> ! {
    let ramdisk = if base == 0 || len == 0 {
        &[][..]
    } else {
        // SAFETY: Per this function's contract, the kernel mapped `len`
        // read-only bytes at `base` for the lifetime of the process.
        unsafe { core::slice::from_raw_parts(base as *const u8, len as usize) }
    };
    sys::exit(main(StartInfo { ramdisk }))
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
