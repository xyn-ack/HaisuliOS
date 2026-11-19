// SPDX-License-Identifier: MIT

//! The HaisuliOS user-space runtime.
//!
//! Everything a HaisuliOS program needs to start, make system calls, print and panic.
//!
//! It provides the program entry point ([`entry!`]), system calls ([`sys`]), threads
//! ([`thread`]), buffered console output ([`print!`] / [`println!`]), device access for drivers
//! ([`device`]) and a panic handler. Programs are `#![no_std]`, `#![no_main]` static executables
//! linked at `USER_BASE` by `user/user.ld`.
//!
//! The runtime is the same on every architecture, but for `arch`, which holds all of user
//! space's assembly: the system call instruction, `_start`, the thread and signal-return stubs,
//! and port I/O.

#![no_std]

pub use haisuli_abi as abi;

mod arch;
pub mod device;
pub mod io;
pub mod sys;
pub mod thread;

/// What a new program is started with: its arguments and environment.
///
/// The kernel leaves them on the initial stack, in the System V layout (see
/// [`abi::exec`]).
#[non_exhaustive]
pub struct StartInfo {
    /// The arguments (`argv`).
    args: Strings,
    /// The environment strings (`envp`).
    env: Strings,
}

impl StartInfo {
    /// Reads the initial stack at `sp`.
    ///
    /// Anything that does not lie within the stack (which a correct kernel
    /// never produces) counts as no arguments.
    fn from_stack(sp: u64) -> StartInfo {
        let argc = stack_word(sp, sp).unwrap_or(0) as usize;
        let argv = sp + 8;
        let envp = argv + (argc as u64 + 1) * 8;
        let envc = (0..)
            .take_while(|&i| stack_word(sp, envp + i * 8).is_some_and(|p| p != 0))
            .count();
        StartInfo {
            args: Strings {
                sp,
                pointers: argv,
                count: argc,
            },
            env: Strings {
                sp,
                pointers: envp,
                count: envc,
            },
        }
    }

    /// The program's arguments, starting with its name (`argv[0]`).
    pub fn args(&self) -> Strings {
        self.args
    }

    /// The program's environment strings (`NAME=value`).
    pub fn env(&self) -> Strings {
        self.env
    }
}

/// Strings that the kernel left on the initial stack: the arguments or the environment.
///
/// Strings that are not valid UTF-8 come out as `"?"`.
#[derive(Clone, Copy)]
pub struct Strings {
    /// The initial stack pointer: the strings lie between it and the stack's top.
    sp: u64,
    /// Where the first pointer is.
    pointers: u64,
    /// How many strings there are.
    count: usize,
}

impl Iterator for Strings {
    type Item = &'static str;

    fn next(&mut self) -> Option<&'static str> {
        if self.count == 0 {
            return None;
        }
        let ptr = stack_word(self.sp, self.pointers)?;
        self.pointers += 8;
        self.count -= 1;
        Some(stack_string(self.sp, ptr).map_or("?", |s| core::str::from_utf8(s).unwrap_or("?")))
    }
}

/// The word at `addr`, if it lies in the initial stack area `[sp, USER_STACK_TOP)`.
fn stack_word(sp: u64, addr: u64) -> Option<u64> {
    let end = addr.checked_add(8)?;
    if addr < sp || end > abi::USER_STACK_TOP || !addr.is_multiple_of(8) {
        return None;
    }
    // SAFETY: The initial stack area is mapped and was filled by the kernel
    // before the program started; the program's own stack grows down from
    // below `sp`, so nothing ever writes there. `addr` is aligned.
    Some(unsafe { core::ptr::read(addr as *const u64) })
}

/// The NUL-terminated string at `addr`, if it lies in the initial stack area
/// `[sp, USER_STACK_TOP)`.
fn stack_string(sp: u64, addr: u64) -> Option<&'static [u8]> {
    if addr < sp || addr >= abi::USER_STACK_TOP {
        return None;
    }
    let room = (abi::USER_STACK_TOP - addr) as usize;
    // SAFETY: As in `stack_word`: the bytes from `addr` to the top of the
    // stack are mapped, initialised and never written again, so they can be
    // borrowed for the rest of the program.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(addr as *const u8, room) };
    let len = bytes.iter().position(|&b| b == 0)?;
    Some(&bytes[..len])
}

/// Define the program's entry point: `entry!(main)` with
/// `fn main(info: StartInfo) -> i32`; the return value is the exit status.
///
/// The expansion contains no unsafe blocks, only the architecture's `_start` (see `arch`): it
/// sets up a proper call frame and passes the address of `main` and the initial stack pointer
/// (where the arguments are) to `__start`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        // Compile-time check that `main` has the signature `__start` expects.
        const _: fn($crate::StartInfo) -> i32 = $main;

        $crate::__haisuli_start!($main);
    };
}

/// Runtime entry, called only by the `_start` that `entry!` defines, with the
/// initial stack pointer `sp`.
#[doc(hidden)]
// `main` is a Rust function called by Rust code below; the assembly only
// passes its address through (a pointer-sized, non-null value), and `entry!`
// checks its type at compile time.
#[expect(improper_ctypes_definitions)]
pub extern "C" fn __start(main: fn(StartInfo) -> i32, sp: u64) -> ! {
    sys::exit(main(StartInfo::from_stack(sp)))
}

/// Panics are reported on the kernel debug log, which works even when the
/// console server is unavailable (or is the process that panicked).
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    use core::fmt::Write;
    let mut w = io::BufWriter::new(io::Sink::DebugLog);
    // The task id, not the pid: asking the process server may not work
    // (the process server itself may be the one panicking).
    let _ = writeln!(w, "[task {}] panic: {}", sys::task_self(), info);
    w.flush();
    sys::exit(101)
}
