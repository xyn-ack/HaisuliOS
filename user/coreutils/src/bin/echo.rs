// SPDX-License-Identifier: GPL-2.0

//! `echo`.
//!
//! Prints its arguments, separated by spaces, and a newline.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use haisuli_rt::{print, println};

haisuli_rt::entry!(main);

fn main(info: haisuli_rt::StartInfo) -> i32 {
    for (i, arg) in info.args().skip(1).enumerate() {
        print!("{}{}", if i == 0 { "" } else { " " }, arg);
    }
    println!();
    0
}
