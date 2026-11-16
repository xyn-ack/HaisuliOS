// SPDX-License-Identifier: GPL-2.0

//! `nproc`.
//!
//! Prints the number of CPUs that are online, as GNU coreutils' `nproc` does.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use haisuli_rt::{println, sys};

haisuli_rt::entry!(main);

fn main(_: haisuli_rt::StartInfo) -> i32 {
    println!("{}", sys::nprocs());
    0
}
