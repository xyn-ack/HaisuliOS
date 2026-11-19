// SPDX-License-Identifier: MIT

//! `false`.
//!
//! Does nothing, unsuccessfully: exits with status 1.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

haisuli_rt::entry!(main);

fn main(_: haisuli_rt::StartInfo) -> i32 {
    1
}
