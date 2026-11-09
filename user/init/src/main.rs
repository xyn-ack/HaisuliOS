// SPDX-License-Identifier: GPL-2.0

//! HaisuliOS init.
//!
//! The first process (pid 1): shows the message of the day, then runs an interactive shell.
//!
//! The message of the day comes from `/etc/motd` on the boot ramdisk. Until there is a file system
//! server, init reads the ramdisk (a ustar archive the kernel mapped into its address space)
//! directly. When init exits, the system shuts down.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::fmt::Write;
use haisuli_rt::io::{write_all, BufWriter, Sink, STDIN, STDOUT};
use haisuli_rt::{print, println, sys};

mod ustar;

haisuli_rt::entry!(main);

const MOTD: &str = "/etc/motd";
const PROMPT: &str = "haisuli# ";

fn main(info: haisuli_rt::StartInfo) -> i32 {
    let fs = ustar::Archive::new(info.ramdisk);
    match fs.find(MOTD) {
        Some(f) if f.is_file() => {
            let _ = write_all(STDOUT, f.data);
        }
        _ => println!("init: {} not found on the boot ramdisk", MOTD),
    }

    let mut buf = [0u8; 256];
    loop {
        print!("{}", PROMPT);
        let n = match sys::read(STDIN, &mut buf) {
            Ok(0) => {
                println!();
                return 0; // End of input (Ctrl-D).
            }
            Ok(n) => n,
            Err(e) => {
                // The console itself is broken, so report on the kernel log.
                let mut w = BufWriter::new(Sink::DebugLog);
                let _ = writeln!(w, "init: console read failed (errno {}), giving up", e.0);
                w.flush();
                return 1;
            }
        };
        let Ok(line) = core::str::from_utf8(&buf[..n]) else {
            println!("init: input is not valid UTF-8");
            continue;
        };
        if let Some(status) = run(&fs, line.trim()) {
            return status;
        }
    }
}

/// Executes one command line.
///
/// Returns `Some(status)` when the shell should exit.
fn run(fs: &ustar::Archive<'_>, line: &str) -> Option<i32> {
    let mut words = line.split_ascii_whitespace();
    let cmd = words.next()?; // An empty line.
    match cmd {
        "help" => {
            println!("commands:");
            println!("  help            this list");
            println!("  echo [text...]  print text");
            println!("  ls [dir]        list files on the boot ramdisk");
            println!("  cat file...     print files");
            println!("  exit            leave the shell and power off");
        }
        "echo" => {
            let mut first = true;
            for w in words {
                print!("{}{}", if first { "" } else { " " }, w);
                first = false;
            }
            println!();
        }
        "ls" => ls(fs, words.next().unwrap_or("/")),
        "cat" => {
            for path in words {
                match fs.find(path) {
                    Some(f) if f.is_file() => {
                        let _ = write_all(STDOUT, f.data);
                    }
                    Some(_) => println!("cat: {}: is a directory", path),
                    None => println!("cat: {}: no such file", path),
                }
            }
        }
        "exit" => return Some(0),
        _ => println!("{}: command not found (try 'help')", cmd),
    }
    None
}

fn ls(fs: &ustar::Archive<'_>, arg: &str) {
    let dir = ustar::normalize(arg);
    // Archives need not contain entries for directories, only for files.
    let is_dir = fs.find(dir).is_some_and(|e| !e.is_file())
        || fs
            .entries()
            .any(|e| ustar::child_of(dir, e.path()).is_some());
    if !is_dir {
        println!("ls: {}: no such directory", arg);
        return;
    }
    for e in fs.entries() {
        if let Some(name) = ustar::child_of(dir, e.path()) {
            println!("{}{}", name, if e.is_file() { "" } else { "/" });
        }
    }
}
