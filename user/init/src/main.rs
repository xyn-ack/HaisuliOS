// SPDX-License-Identifier: GPL-2.0

//! HaisuliOS init.
//!
//! The first process (pid 1): shows the message of the day, then runs an interactive shell.
//!
//! The message of the day comes from `/etc/motd` on the boot ramdisk. Until there is a file system
//! server, init reads the ramdisk (a ustar archive the kernel mapped into its address space)
//! directly. When init exits, the system shuts down.
//!
//! The shell is a job control shell: `cmd &` runs a command in the background, `pause` runs in
//! the foreground as a job that Ctrl-C interrupts, and `$?`, `$!`, `$$`, `wait` and `kill` work
//! as in a POSIX shell (see [`jobs`]). Ctrl-C at the prompt discards the line and shows a new
//! prompt.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};
use haisuli_rt::abi::errno::EINTR;
use haisuli_rt::abi::signal::{SIGINT, SIGQUIT, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU};
use haisuli_rt::io::{write_all, BufWriter, Sink, STDIN, STDOUT};
use haisuli_rt::sys::{self, Errno, Handler};
use haisuli_rt::{print, println};

mod jobs;
mod signals;
mod ustar;

haisuli_rt::entry!(main);

const MOTD: &str = "/etc/motd";
const PROMPT: &str = "haisuli# ";
/// Longest command line after expanding `$?`, `$!` and `$$`.
const EXPANDED_MAX: usize = 1024;
/// Most words in a command line.
const MAX_WORDS: usize = 64;

/// The exit status of a command that does not exist.
const STATUS_NOT_FOUND: i32 = 127;

fn main(info: haisuli_rt::StartInfo) -> i32 {
    let fs = ustar::Archive::new(info.ramdisk);
    match fs.find(MOTD) {
        Some(f) if f.is_file() => {
            let _ = write_all(STDOUT, f.data);
        }
        _ => println!("init: {} not found on the boot ramdisk", MOTD),
    }

    // An interactive shell survives the keyboard's signals: Ctrl-C only interrupts what it is
    // waiting for (reading a line, `wait`). Jobs get the default actions back.
    let _ = sys::sigaction(SIGINT, Handler::Catch(on_interrupt), 0, 0);
    for sig in [SIGQUIT, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU] {
        let _ = sys::sigaction(sig, Handler::Ignore, 0, 0);
    }
    jobs::take_terminal();

    let mut sh = Shell {
        fs,
        pid: sys::getpid(),
        status: 0,
        last_background: None,
        jobs: jobs::Jobs::new(),
    };
    let mut buf = [0u8; 256];
    loop {
        sh.jobs.report();
        INTERRUPTED.store(false, Ordering::Relaxed);
        print!("{}", PROMPT);
        // Ctrl-C while the prompt was being written: the console has discarded the line, and
        // the read would not notice. (Signals arrive only when a system call returns, so none
        // can slip in between this check and the read.)
        if INTERRUPTED.load(Ordering::Relaxed) {
            continue;
        }
        let n = match sys::read(STDIN, &mut buf) {
            Ok(0) => {
                println!();
                return sh.status; // End of input (Ctrl-D).
            }
            Ok(n) => n,
            // Ctrl-C: the console has discarded the line; start over.
            Err(Errno(EINTR)) => continue,
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
            sh.status = 1;
            continue;
        };
        if let Some(status) = sh.run_line(line.trim()) {
            return status;
        }
    }
}

/// Set by Ctrl-C (`SIGINT`), which the shell survives.
pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// The shell's `SIGINT` handler.
///
/// Besides setting [`INTERRUPTED`], its existence makes Ctrl-C interrupt a blocking call
/// (which then fails with `EINTR`) instead of terminating the shell.
extern "C" fn on_interrupt(_sig: i32) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// What a command did.
enum Flow {
    /// It finished with this exit status.
    Status(i32),
    /// The shell (or the job running the command) exits with this status.
    Exit(i32),
}

struct Shell<'a> {
    fs: ustar::Archive<'a>,
    /// `$$`.
    pid: u64,
    /// `$?`: the exit status of the last command.
    status: i32,
    /// `$!`: the pid of the last background job.
    last_background: Option<u64>,
    jobs: jobs::Jobs,
}

impl Shell<'_> {
    /// Expands and executes one command line.
    ///
    /// Returns `Some(status)` when the shell should exit.
    fn run_line(&mut self, line: &str) -> Option<i32> {
        let mut expanded = [0u8; EXPANDED_MAX];
        let Some(line) = self.expand(line, &mut expanded) else {
            println!("init: command line too long");
            self.status = 1;
            return None;
        };
        let mut words = [""; MAX_WORDS];
        let mut n = 0;
        for w in line.split_ascii_whitespace() {
            if n == MAX_WORDS {
                println!("init: too many words");
                self.status = 1;
                return None;
            }
            words[n] = w;
            n += 1;
        }
        // A trailing `&` (a word of its own, or the end of the last word): run in the background.
        let mut background = false;
        if let Some(last) = words[..n].last_mut() {
            if let Some(rest) = last.strip_suffix('&') {
                background = true;
                *last = rest;
                if rest.is_empty() {
                    n -= 1;
                }
            }
        }
        let argv = &words[..n];
        let Some(&cmd) = argv.first() else {
            if background {
                println!("init: syntax error near unexpected token `&'");
                self.status = 2;
            }
            return None; // An empty line.
        };
        // `pause` waits for a signal, so it runs as a job of its own, which Ctrl-C interrupts.
        if background || cmd == "pause" {
            let (fs, status) = (&self.fs, self.status);
            self.status = match self.jobs.start(cmd, background, || {
                match execute(fs, status, &mut jobs::Jobs::new(), argv) {
                    Flow::Status(s) | Flow::Exit(s) => s,
                }
            }) {
                jobs::Started::Foreground(status) => status,
                jobs::Started::Background(pid) => {
                    self.last_background = Some(pid);
                    0
                }
                jobs::Started::Failed => 1,
            };
            return None;
        }
        match execute(&self.fs, self.status, &mut self.jobs, argv) {
            Flow::Status(s) => {
                self.status = s;
                None
            }
            Flow::Exit(s) => Some(s),
        }
    }

    /// Expands `$?`, `$!` and `$$` in `line` into `buf`; `None` if it does not fit.
    fn expand<'b>(&self, line: &str, buf: &'b mut [u8; EXPANDED_MAX]) -> Option<&'b str> {
        let mut w = Cursor { buf, len: 0 };
        let mut rest = line;
        while let Some(i) = rest.find('$') {
            w.write_str(&rest[..i]).ok()?;
            let after = &rest[i + 1..];
            match after.as_bytes().first() {
                Some(b'?') => write!(w, "{}", self.status).ok()?,
                Some(b'!') => {
                    if let Some(pid) = self.last_background {
                        write!(w, "{}", pid).ok()?;
                    }
                }
                Some(b'$') => write!(w, "{}", self.pid).ok()?,
                _ => {
                    w.write_str("$").ok()?;
                    rest = after;
                    continue;
                }
            }
            rest = &after[1..];
        }
        w.write_str(rest).ok()?;
        let Cursor { buf, len } = w;
        core::str::from_utf8(&buf[..len]).ok()
    }
}

/// A [`Write`] into a fixed buffer that fails when the buffer is full.
struct Cursor<'b> {
    buf: &'b mut [u8],
    len: usize,
}

impl Write for Cursor<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Executes the command `argv` (not empty), in the shell or in a job.
///
/// `status` is `$?`, for `exit` without an argument.
fn execute(fs: &ustar::Archive<'_>, status: i32, jobs: &mut jobs::Jobs, argv: &[&str]) -> Flow {
    let (cmd, args) = (argv[0], &argv[1..]);
    Flow::Status(match cmd {
        "help" => {
            help();
            0
        }
        "echo" => {
            let mut first = true;
            for w in args {
                print!("{}{}", if first { "" } else { " " }, w);
                first = false;
            }
            println!();
            0
        }
        "ls" => ls(fs, args.first().copied().unwrap_or("/")),
        "cat" => {
            let mut status = 0;
            for path in args {
                match fs.find(path) {
                    Some(f) if f.is_file() => {
                        let _ = write_all(STDOUT, f.data);
                    }
                    Some(_) => {
                        println!("cat: {}: is a directory", path);
                        status = 1;
                    }
                    None => {
                        println!("cat: {}: no such file", path);
                        status = 1;
                    }
                }
            }
            status
        }
        "ps" => ps(),
        "pause" => {
            sys::pause();
            0
        }
        "kill" => signals::kill(args),
        "wait" => jobs.wait(args),
        "exit" => {
            return match args.first().map(|a| a.parse::<u8>()) {
                None => Flow::Exit(status),
                Some(Ok(code)) => Flow::Exit(code as i32),
                Some(Err(_)) => {
                    println!("exit: {}: numeric argument required", args[0]);
                    Flow::Status(2)
                }
            };
        }
        _ => {
            println!("{}: command not found (try 'help')", cmd);
            STATUS_NOT_FOUND
        }
    })
}

fn help() {
    println!("commands:");
    println!("  help                this list");
    println!("  echo [text...]      print text");
    println!("  ls [dir]            list files on the boot ramdisk");
    println!("  cat file...         print files");
    println!("  ps                  list processes");
    println!("  pause               wait for a signal (Ctrl-C interrupts it)");
    println!("  kill [-sig] pid...  send a signal (default TERM; -l lists them)");
    println!("  wait [pid...]       wait for background jobs");
    println!("  exit [n]            leave the shell and power off");
    println!("'command &' runs a command in the background. $? is the exit status of the");
    println!("last command, $! the pid of the last background job and $$ the shell's pid.");
}

fn ls(fs: &ustar::Archive<'_>, arg: &str) -> i32 {
    let dir = ustar::normalize(arg);
    // Archives need not contain entries for directories, only for files.
    let is_dir = fs.find(dir).is_some_and(|e| !e.is_file())
        || fs
            .entries()
            .any(|e| ustar::child_of(dir, e.path()).is_some());
    if !is_dir {
        println!("ls: {}: no such directory", arg);
        return 1;
    }
    for e in fs.entries() {
        if let Some(name) = ustar::child_of(dir, e.path()) {
            println!("{}{}", name, if e.is_file() { "" } else { "/" });
        }
    }
    0
}

/// `ps`: lists every process.
fn ps() -> i32 {
    let mut w = BufWriter::new(Sink::Fd(STDOUT));
    let _ = writeln!(w, "  PID  PPID  PGID   SID STAT COMMAND");
    let mut next = 1;
    while let Ok(p) = sys::proc_info(next) {
        let _ = writeln!(
            w,
            "{:5} {:5} {:5} {:5} {:<4} {}",
            p.pid,
            p.ppid,
            p.pgid,
            p.sid,
            p.state as char,
            p.name()
        );
        next = p.pid + 1;
    }
    w.flush();
    0
}
