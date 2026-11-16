// SPDX-License-Identifier: GPL-2.0

//! `echo`.
//!
//! Prints its arguments, separated by spaces, and a newline.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::fmt::Write;
use haisuli_rt::io::{BufWriter, Sink, STDOUT};

haisuli_rt::entry!(main);

fn main(info: haisuli_rt::StartInfo) -> i32 {
    // One write for the line (as far as the buffer goes), so that other output on the terminal
    // cannot cut into it.
    let mut out = BufWriter::new(Sink::Fd(STDOUT));
    for (i, arg) in info.args().skip(1).enumerate() {
        let _ = write!(out, "{}{}", if i == 0 { "" } else { " " }, arg);
    }
    let _ = out.write_str("\n");
    out.flush();
    0
}
