// SPDX-License-Identifier: GPL-2.0

//! The CPU architecture's part of the runtime: all the assembly of user space.
//!
//! One file per architecture, selected at compile time; each provides the same items:
//!
//! - `syscall`: the system call instruction, with the architecture's registers;
//! - the `__haisuli_start!` macro: the program's `_start`, which [`crate::entry!`] expands to,
//!   and which calls [`crate::__start`] with the program's `main` and the initial stack pointer;
//! - `clone_thread` and `thread_exit`: a thread's start on its own stack, and its end without
//!   that stack ([`crate::thread`]);
//! - `sigreturn_restorer`: where every signal handler returns to, which hands the saved signal
//!   mask to [`crate::sys::restore_signal_mask`] and makes the `rt_sigreturn` call;
//! - on x86_64 only, `ports`: I/O port access, for the devices granted to the program
//!   ([`crate::device`]).
//!
//! Nothing else in user space uses assembly.

#[cfg(target_arch = "aarch64")]
#[path = "aarch64.rs"]
mod imp;
#[cfg(target_arch = "x86_64")]
#[path = "x86_64.rs"]
mod imp;

#[cfg(target_arch = "x86_64")]
pub(crate) use imp::ports;
pub(crate) use imp::{clone_thread, sigreturn_restorer, syscall, thread_exit};
