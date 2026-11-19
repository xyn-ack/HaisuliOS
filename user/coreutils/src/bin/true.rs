// SPDX-License-Identifier: MIT

//! `true`.
//!
//! Does nothing, successfully: exits with status 0.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

haisuli_rt::entry!(main);

fn main(_: haisuli_rt::StartInfo) -> i32 {
    0
}
