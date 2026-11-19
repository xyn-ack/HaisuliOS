// SPDX-License-Identifier: MIT

//! HaisuliOS init.
//!
//! The first process (pid 1): shows the message of the day, then runs an interactive shell.
//!
//! The message of the day comes from `/etc/motd`, from the boot ramdisk, which the kernel unpacks
//! into its file system. When init exits, the system shuts down.
//!
//! The shell does POSIX job control: `cmd &` runs a command in a child of its own process group,
//! `$?` is the status of the last command (128 + n if signal n killed it), and `wait`, `kill` and
//! `ps` manage the running processes. A foreground job owns the terminal while it runs, so Ctrl-C
//! interrupts the job rather than the shell: the console server sends SIGINT to the foreground
//! process group. Between jobs that is the shell's own group; the shell catches SIGINT with a real
//! signal handler, which interrupts the line being read. `trap` attaches commands to signals (see
//! [`traps`]).
//!
//! Commands take POSIX redirections (`>`, `>>`, `<`, `2>&1`; see [`redirect`]) and report errors on
//! standard error. A command that is not built in runs a program from the file system: one named
//! by its path, or by its name in `/bin` ([`program_path`]), in a child that `execve`s it. Most
//! built-in commands are quick and run in the shell itself, like the special built-ins of a POSIX
//! shell; commands that wait for something (see [`runs_in_child`]) always run in a child, so that
//! they can be signalled.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::fmt::Write;
use haisuli_rt::abi::errno::{self, ECHILD, EINTR, EISDIR, ENOENT, ENOTDIR, EPERM, EPIPE, ESRCH};
use haisuli_rt::abi::fcntl::{O_DIRECTORY, O_RDONLY, PATH_MAX};
use haisuli_rt::abi::proc_state;
use haisuli_rt::abi::signal::{self, NSIG, SIGINT, SIGTERM, SIG_BLOCK, SIG_SETMASK};
use haisuli_rt::abi::{ProcInfo, Termination};
use haisuli_rt::io::{tcsetpgrp, write_all, BufWriter, Sink, STDIN, STDOUT};
use haisuli_rt::{eprintln, print, println, sys};

mod redirect;
mod traps;
mod words;

haisuli_rt::entry!(main);

const MOTD: &str = "/etc/motd";
const PROMPT: &str = "haisuli# ";
/// Background jobs the shell remembers at a time.
const MAX_JOBS: usize = 8;
/// Every signal, as a signal set (SIGKILL and SIGSTOP are never blocked anyway).
const ALL_SIGNALS: u64 = (1 << (NSIG - 1)) - 1;
/// Status of a command that could not be run, as POSIX prescribes.
const STATUS_NOT_FOUND: i32 = 127;
/// Status of a program that was found but could not be run, as POSIX prescribes.
const STATUS_NOT_EXECUTABLE: i32 = 126;
/// Where the shell looks for a program named without a path (`PATH`).
const PROGRAM_DIR: &str = "/bin/";
/// The built-in commands.
const BUILTINS: [&str; 10] = [
    "help", "echo", "ls", "cat", "ps", "pause", "kill", "wait", "trap", "exit",
];

fn main(_: haisuli_rt::StartInfo) -> i32 {
    match sys::open(MOTD, O_RDONLY) {
        Ok(fd) => {
            let _ = copy(fd, STDOUT);
            let _ = sys::close(fd);
        }
        Err(_) => eprintln!("init: {} not found on the boot ramdisk", MOTD),
    }
    Shell::new().run()
}

/// Why copying between descriptors stopped early.
enum CopyError {
    /// Reading failed (e.g. `EISDIR`, or `EINTR` for Ctrl-C).
    Read(sys::Errno),
    /// Writing failed.
    Write(sys::Errno),
}

/// Copies everything readable from `from` to `to`.
fn copy(from: u64, to: u64) -> Result<(), CopyError> {
    let mut buf = [0u8; 512];
    loop {
        match sys::read(from, &mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => write_all(to, &buf[..n]).map_err(CopyError::Write)?,
            Err(e) => return Err(CopyError::Read(e)),
        }
    }
}

/// A background job: a process group with one process in it.
#[derive(Clone, Copy, Default)]
struct Job {
    pid: u64,
}

/// What running one command produced.
struct Outcome {
    /// The exit status, for `$?`.
    status: i32,
    /// Whether the shell itself should exit with it.
    exit: bool,
}

impl Outcome {
    fn status(status: i32) -> Outcome {
        Outcome {
            status,
            exit: false,
        }
    }
}

struct Shell {
    /// `$?`: the status of the last command.
    status: i32,
    /// `$!`: the pid of the last background job.
    last_bg: u64,
    /// `$$`: the shell's own pid.
    pid: u64,
    /// The shell's process group, which owns the terminal between jobs.
    pgid: u64,
    jobs: [Job; MAX_JOBS],
    /// The number the next background job gets.
    next_job: u32,
    traps: traps::Traps,
    /// Whether a trap command is running (traps do not nest).
    in_trap: bool,
}

impl Shell {
    fn new() -> Shell {
        let pid = sys::getpid();
        // Job control: the shell keeps the terminal while no job runs, and
        // survives the signals the terminal sends to the foreground group.
        let traps = traps::Traps::new();
        let pgid = sys::getpgid(0).unwrap_or(pid);
        let _ = tcsetpgrp(pgid);
        Shell {
            status: 0,
            last_bg: 0,
            pid,
            pgid,
            jobs: [Job::default(); MAX_JOBS],
            next_job: 1,
            traps,
            in_trap: false,
        }
    }

    /// Reads and runs command lines until the shell exits.
    fn run(&mut self) -> i32 {
        let mut buf = [0u8; 256];
        loop {
            if let Some(status) = self.run_traps() {
                return status;
            }
            // The shell holds the terminal while it reads a command, as POSIX shells take it back
            // before every prompt. That also tells a console server that was restarted since the
            // last prompt (and knows nothing of the old one's foreground group).
            let _ = tcsetpgrp(self.pgid);
            print!("{}", PROMPT);
            // Signals are only delivered when a system call returns, so one that arrived while
            // prompting (or taking the terminal) is seen here, before `read` blocks; one that
            // arrives later interrupts the `read`.
            if traps::any_caught() {
                leave_prompt();
                continue;
            }
            let n = match sys::read(STDIN, &mut buf) {
                Ok(0) => {
                    println!();
                    return 0; // End of input (Ctrl-D).
                }
                Ok(n) => n,
                Err(e) if e.0 == EINTR => {
                    leave_prompt();
                    continue;
                }
                // The console server died while we read: the process server starts it again,
                // and the line being typed is lost. Prompt again (which waits for the new one).
                Err(e) if e.0 == EPIPE => {
                    println!();
                    continue;
                }
                Err(e) => {
                    // The console itself is broken, so report on the kernel log.
                    let mut w = BufWriter::new(Sink::DebugLog);
                    let _ = writeln!(w, "init: console read failed (errno {}), giving up", e.0);
                    w.flush();
                    return 1;
                }
            };
            let Ok(line) = core::str::from_utf8(&buf[..n]) else {
                eprintln!("init: input is not valid UTF-8");
                continue;
            };
            if let Some(status) = self.line(line.trim()) {
                return status;
            }
        }
    }

    /// Runs the commands of the traps whose signals were caught.
    ///
    /// POSIX: a trap leaves `$?` as it was. `Some(status)` means a trap made
    /// the shell exit.
    fn run_traps(&mut self) -> Option<i32> {
        if self.in_trap {
            return None;
        }
        let caught = traps::take_caught();
        for sig in (1..NSIG).filter(|&sig| caught & signal::bit(sig) != 0) {
            let mut buf = traps::Traps::buffer();
            let Some(command) = self.traps.command(sig, &mut buf) else {
                continue;
            };
            let status = self.status;
            self.in_trap = true;
            let exit = self.line(command);
            self.in_trap = false;
            self.status = status;
            if exit.is_some() {
                return exit;
            }
        }
        None
    }

    /// Runs one command line; `Some(status)` means the shell should exit.
    fn line(&mut self, line: &str) -> Option<i32> {
        let mut expanded = [0u8; 2 * 256];
        let params = words::Params {
            status: self.status,
            last_bg: self.last_bg,
            pid: self.pid,
        };
        let split = match words::split(line, &mut expanded, &params) {
            Ok(split) => split,
            Err(e) => {
                match e {
                    words::Error::TooMany => eprintln!(
                        "init: too many words in one command (at most {} words and {} redirections)",
                        words::MAX_WORDS,
                        words::MAX_REDIRECTS
                    ),
                    words::Error::Unterminated => eprintln!("init: missing closing quote"),
                    words::Error::MissingTarget => {
                        eprintln!("init: syntax error: a redirection needs a file")
                    }
                }
                // POSIX: 2 for a syntax error.
                self.status = if matches!(e, words::Error::MissingTarget) {
                    2
                } else {
                    1
                };
                return None;
            }
        };
        let (background, words, redirects) = (split.background, split.get(), split.redirects());
        if words.is_empty() && redirects.is_empty() {
            return None; // An empty line leaves `$?` alone.
        }
        if background || words.first().is_some_and(|&w| runs_in_child(w)) {
            self.spawn(words, redirects, background);
            return None;
        }
        // A command of the shell itself: redirect around it.
        let Ok(saved) = redirect::apply(redirects, true) else {
            self.status = 1;
            return None;
        };
        let outcome = if words.is_empty() {
            Outcome::status(0) // Only redirections: they create or empty files.
        } else {
            self.execute(words)
        };
        saved.restore();
        self.status = outcome.status;
        outcome.exit.then_some(outcome.status)
    }

    /// Runs a command in a child process, as a foreground or background job.
    fn spawn(&mut self, words: &[&str], redirects: &[words::Redirect<'_>], background: bool) {
        // Until the child has reset the shell's signal actions, a signal sent to it would meet
        // the shell's (ignored, or the shell's own handler): so every signal is blocked from
        // before the fork until then, and stays pending meanwhile.
        let old_mask = sys::sigprocmask(SIG_BLOCK, ALL_SIGNALS).unwrap_or(0);
        let pid = match sys::fork() {
            Ok(pid) => pid,
            Err(e) => {
                let _ = sys::sigprocmask(SIG_SETMASK, old_mask);
                eprintln!("init: cannot fork: {}", errno::describe(e.0));
                self.status = 1;
                return;
            }
        };
        if pid == 0 {
            // The child is a new process group, with the shell's signals back
            // at their default disposition so that it can be signalled.
            let _ = sys::setpgid(0, 0);
            self.traps.reset_for_job();
            let _ = sys::sigprocmask(SIG_SETMASK, old_mask);
            if redirect::apply(redirects, false).is_err() {
                sys::exit(1);
            }
            let status = if words.is_empty() {
                0
            } else {
                self.execute(words).status
            };
            sys::exit(status);
        }
        let _ = sys::sigprocmask(SIG_SETMASK, old_mask);
        // Both sides set the group, so the job is in it whoever runs first.
        let _ = sys::setpgid(pid, pid);
        if background {
            let number = self.add_job(pid);
            self.last_bg = pid;
            println!("[{}] {}", number, pid);
            self.status = 0;
        } else {
            // Hand the terminal to the job, and take it back when it is done.
            // A signal the shell catches meanwhile does not end the job, so
            // keep waiting; its trap runs once the job is done.
            let _ = tcsetpgrp(pid);
            self.status = loop {
                match sys::waitpid(pid as i64, 0) {
                    Ok((_, status)) => break status.shell_status(),
                    Err(e) if e.0 == EINTR => {} // Not the job's signal: keep waiting.
                    Err(_) => break STATUS_NOT_FOUND,
                }
            };
            let _ = tcsetpgrp(self.pgid);
        }
    }

    fn add_job(&mut self, pid: u64) -> u32 {
        let number = self.next_job;
        self.next_job += 1;
        // More jobs than the shell can remember still run; they just cannot be
        // listed, and `wait` without operands still waits for them.
        if let Some(slot) = self.jobs.iter_mut().find(|j| j.pid == 0) {
            *slot = Job { pid };
        }
        number
    }

    fn remove_job(&mut self, pid: u64) {
        for job in self.jobs.iter_mut().filter(|j| j.pid == pid) {
            *job = Job::default();
        }
        if self.jobs.iter().all(|j| j.pid == 0) {
            self.next_job = 1; // Numbering starts over once nothing is left.
        }
    }

    /// Runs one command, in the shell or in a freshly forked child.
    fn execute(&mut self, words: &[&str]) -> Outcome {
        let args = &words[1..];
        match words[0] {
            "help" => {
                println!("commands:");
                println!("  help              this list");
                println!("  echo [text...]    print text");
                println!("  ls [dir]          list a directory");
                println!("  cat [file...]     print files (or the input)");
                println!("  ps                list processes");
                println!("  pause             wait for a signal (as a job: pause &)");
                println!("  kill [-sig] pid   signal a process (-pid: a process group)");
                println!("  wait [pid...]     wait for background jobs");
                println!("  trap [cmd sig...] run cmd on a signal ('' ignores, - resets)");
                println!("  exit [status]     leave the shell and power off");
                println!("  program [args...] run a program in /bin (or give its path)");
                println!("end a command with & to run it in the background; $? is its status");
                println!("redirect with > file, >> file, < file, 2> file and 2>&1");
                Outcome::status(0)
            }
            "echo" => {
                // One write for the line (as far as the buffer goes), so that other output on
                // the terminal, such as the log, cannot cut into it.
                let mut out = BufWriter::new(Sink::Fd(STDOUT));
                for (i, w) in args.iter().enumerate() {
                    let _ = write!(out, "{}{}", if i == 0 { "" } else { " " }, w);
                }
                let _ = out.write_str("\n");
                out.flush();
                Outcome::status(0)
            }
            "ls" => Outcome::status(self.ls(args.first().copied().unwrap_or("/"))),
            "cat" => Outcome::status(self.cat(args)),
            "ps" => Outcome::status(ps()),
            "pause" => {
                let e = sys::pause();
                eprintln!("pause: {}", errno::describe(e.0));
                Outcome::status(1)
            }
            "kill" => Outcome::status(kill(args)),
            "wait" => Outcome::status(self.wait(args)),
            "trap" => Outcome::status(self.trap(args)),
            "exit" => Outcome {
                // POSIX: `exit` without a status uses the status of the last
                // command.
                status: args.first().map_or(self.status, |a| parse_u64(a) as i32),
                exit: true,
            },
            cmd => {
                let mut path = [0u8; PATH_MAX];
                match program_path(cmd, &mut path) {
                    // Only ever in a child (see `runs_in_child`).
                    Some(path) => Outcome::status(run_program(path, words)),
                    None => {
                        eprintln!("{}: command not found (try 'help')", cmd);
                        Outcome::status(STATUS_NOT_FOUND)
                    }
                }
            }
        }
    }

    /// `ls [dir]`: lists a directory, marking subdirectories with `/`.
    fn ls(&self, arg: &str) -> i32 {
        let Ok(fd) = sys::open(arg, O_RDONLY | O_DIRECTORY) else {
            eprintln!("ls: {}: no such directory", arg);
            return 1;
        };
        let mut buf = [0u8; 512];
        let mut status = 0;
        loop {
            match sys::getdents(fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for (name, is_dir) in sys::DirEntries::new(&buf[..n]) {
                        let name = core::str::from_utf8(name).unwrap_or("?");
                        println!("{}{}", name, if is_dir { "/" } else { "" });
                    }
                }
                Err(e) => {
                    eprintln!("ls: {}: {}", arg, errno::describe(e.0));
                    status = 1;
                    break;
                }
            }
        }
        let _ = sys::close(fd);
        status
    }

    /// `cat [file...]`: prints files, or standard input without operands.
    fn cat(&self, paths: &[&str]) -> i32 {
        if paths.is_empty() {
            return cat_one(STDIN, "-");
        }
        let mut status = 0;
        for path in paths {
            let result = match sys::open(path, O_RDONLY) {
                Ok(fd) => {
                    let result = cat_one(fd, path);
                    let _ = sys::close(fd);
                    result
                }
                Err(e) => {
                    match e.0 {
                        ENOENT | ENOTDIR => eprintln!("cat: {}: no such file", path),
                        errno => eprintln!("cat: {}: {}", path, errno::describe(errno)),
                    }
                    1
                }
            };
            status = status.max(result);
        }
        status
    }

    /// `wait [pid...]`: waits for children and reports the last one's status.
    ///
    /// Without operands it waits for every child. A pid that is not a child of
    /// the shell (including one already waited for) gives status 127, as POSIX
    /// requires.
    fn wait(&mut self, args: &[&str]) -> i32 {
        if args.is_empty() {
            loop {
                match sys::waitpid(-1, 0) {
                    Ok((pid, _)) => self.remove_job(pid),
                    Err(e) if e.0 == EINTR => return interrupted_status(),
                    Err(_) => return 0,
                }
            }
        }
        let mut status = 0;
        for arg in args {
            match sys::waitpid(parse_u64(arg) as i64, 0) {
                Ok((pid, reaped)) => {
                    self.remove_job(pid);
                    status = reaped.shell_status();
                }
                Err(e) if e.0 == EINTR => return interrupted_status(),
                Err(e) => {
                    if e.0 == ECHILD {
                        eprintln!("wait: {}: not a child of this shell", arg);
                    } else {
                        eprintln!("wait: {}: cannot wait (errno {})", arg, e.0);
                    }
                    status = STATUS_NOT_FOUND;
                }
            }
        }
        status
    }
}

impl Shell {
    /// `trap [action signal...]`: sets what the shell does on a signal.
    ///
    /// The action is a command, `''` to ignore the signal or `-` to restore
    /// the shell's default handling. Without operands, lists the traps.
    fn trap(&mut self, args: &[&str]) -> i32 {
        let Some((action, signals)) = args.split_first() else {
            self.traps.list();
            return 0;
        };
        if signals.is_empty() {
            eprintln!("trap: usage: trap [action signal...]");
            return 2;
        }
        let mut status = 0;
        for spec in signals {
            let Some(sig) = parse_signal(spec).filter(|&sig| sig != 0) else {
                eprintln!("trap: {}: invalid signal specification", spec);
                status = 1;
                continue;
            };
            if let Err(why) = self.traps.set(sig, action) {
                eprintln!("trap: {}: {}", signal::NAMES[sig as usize], why);
                status = 1;
            }
        }
        status
    }
}

/// Ends the prompt's line after a caught signal, before traps and a new prompt.
///
/// Ctrl-C already did: the terminal showed `^C` and a newline, and dropped the
/// line being typed.
fn leave_prompt() {
    if !traps::caught(SIGINT) {
        println!();
    }
}

/// The status of a `wait` that a caught signal interrupted.
///
/// POSIX: greater than 128; like other shells, 128 plus the signal number.
fn interrupted_status() -> i32 {
    Termination::Signalled(traps::first_caught().unwrap_or(SIGINT)).shell_status()
}

/// Copies `fd` (named `name` in errors) to standard output, for `cat`.
fn cat_one(fd: u64, name: &str) -> i32 {
    match copy(fd, STDOUT) {
        Ok(()) => 0,
        Err(CopyError::Read(e)) if e.0 == EISDIR => {
            eprintln!("cat: {}: is a directory", name);
            1
        }
        // Ctrl-C: the terminal already showed ^C.
        Err(CopyError::Read(e)) if e.0 == EINTR => 1,
        Err(CopyError::Read(e)) => {
            eprintln!("cat: {}: {}", name, errno::describe(e.0));
            1
        }
        // The output is the problem, not the file being read.
        Err(CopyError::Write(e)) => {
            eprintln!("cat: write error: {}", errno::describe(e.0));
            1
        }
    }
}

/// Commands that always run in a child process.
///
/// `pause` waits for a signal, which only makes sense for a process that is
/// not the shell: as a job it can be stopped, continued and killed. A program
/// replaces the process that runs it. (A command that is neither built in nor
/// a program fails in the shell itself.)
fn runs_in_child(command: &str) -> bool {
    command == "pause"
        || (!BUILTINS.contains(&command) && program_path(command, &mut [0; PATH_MAX]).is_some())
}

/// The file of the program that `command` names, if it exists: the command
/// itself if it contains a `/`, or else the command in [`PROGRAM_DIR`].
///
/// The path is put together in `buf`.
fn program_path<'b>(command: &str, buf: &'b mut [u8; PATH_MAX]) -> Option<&'b str> {
    let (dir, name) = if command.contains('/') {
        ("", command)
    } else {
        (PROGRAM_DIR, command)
    };
    let len = dir.len() + name.len();
    if command.is_empty() || len > buf.len() {
        return None;
    }
    buf[..dir.len()].copy_from_slice(dir.as_bytes());
    buf[dir.len()..len].copy_from_slice(name.as_bytes());
    let path = core::str::from_utf8(&buf[..len]).ok()?;
    let fd = sys::open(path, O_RDONLY).ok()?;
    let _ = sys::close(fd);
    Some(path)
}

/// Runs the program at `path` with arguments `words`, in place of the calling
/// process; returns the status to exit with if it cannot be run.
fn run_program(path: &str, words: &[&str]) -> i32 {
    let e = sys::execve(path, words, &[]);
    eprintln!("{}: {}", words[0], errno::describe(e.0));
    if e.0 == ENOENT {
        STATUS_NOT_FOUND
    } else {
        STATUS_NOT_EXECUTABLE
    }
}

/// `kill [-s signal | -signal] pid...`: sends a signal.
///
/// The default signal is SIGTERM, signal 0 only checks that the target exists,
/// and a negative pid addresses a whole process group.
fn kill(args: &[&str]) -> i32 {
    let mut sig = SIGTERM;
    let mut targets = args;
    match args.first().copied() {
        Some("-s") => match args.get(1).copied().and_then(parse_signal) {
            Some(parsed) => {
                sig = parsed;
                targets = &args[2..];
            }
            None => {
                eprintln!(
                    "kill: {}: invalid signal specification",
                    args.get(1).copied().unwrap_or("")
                );
                return 1;
            }
        },
        // The first argument is the signal; later ones may be negative pids.
        Some(spec) if spec.len() > 1 && spec.starts_with('-') && spec != "--" => {
            match parse_signal(&spec[1..]) {
                Some(parsed) => {
                    sig = parsed;
                    targets = &args[1..];
                }
                None => {
                    eprintln!("kill: {}: invalid signal specification", &spec[1..]);
                    return 1;
                }
            }
        }
        _ => {}
    }
    // `--` ends the options, so that a process group can follow.
    if targets.first() == Some(&"--") {
        targets = &targets[1..];
    }
    if targets.is_empty() {
        eprintln!("kill: usage: kill [-s signal | -signal] [--] pid...");
        return 2;
    }
    let mut status = 0;
    for target in targets {
        let pid = match parse_i64(target) {
            Some(pid) => pid,
            None => {
                eprintln!(
                    "kill: {}: arguments must be process or process group ids",
                    target
                );
                status = 1;
                continue;
            }
        };
        match sys::kill(pid, sig) {
            Ok(()) => {}
            Err(e) => {
                match e.0 {
                    ESRCH => eprintln!("kill: {}: No such process", target),
                    EPERM => eprintln!("kill: {}: Operation not permitted", target),
                    errno => eprintln!("kill: {}: cannot signal (errno {})", target, errno),
                }
                status = 1;
            }
        }
    }
    status
}

/// `ps`: lists every process with its job control state.
fn ps() -> i32 {
    let mut procs = [ProcInfo::default(); sys::PROC_LIST_MAX];
    let n = match sys::proc_list(&mut procs) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("ps: cannot list processes (errno {})", e.0);
            return 1;
        }
    };
    println!(
        "{:>5} {:>5} {:>5} {:>5} {:<4} {:>3} {}",
        "PID", "PPID", "PGID", "SID", "STAT", "THR", "NAME"
    );
    for p in &procs[..n] {
        let stat = match p.state {
            proc_state::RUNNING => "R",
            proc_state::SLEEPING => "S",
            proc_state::STOPPED => "T",
            proc_state::ZOMBIE => "Z",
            _ => "?",
        };
        println!(
            "{:>5} {:>5} {:>5} {:>5} {:<4} {:>3} {}",
            p.pid,
            p.ppid,
            p.pgid,
            p.sid,
            stat,
            p.threads,
            p.name()
        );
    }
    0
}

/// The signal a `kill` argument names: a number or a name such as `TERM`.
fn parse_signal(spec: &str) -> Option<u64> {
    if spec.bytes().all(|b| b.is_ascii_digit()) {
        let n = spec.parse_u64()?;
        (n < signal::NSIG).then_some(n)
    } else {
        signal::by_name(spec)
    }
}

/// Parses a decimal number, or 0 if the text is not one.
fn parse_u64(s: &str) -> u64 {
    s.parse_u64().unwrap_or(0)
}

/// Parses a pid or a negative process group id.
fn parse_i64(s: &str) -> Option<i64> {
    match s.strip_prefix('-') {
        Some(digits) => digits.parse_u64().map(|n| -(n as i64)),
        None => s.parse_u64().map(|n| n as i64),
    }
}

/// Parsing of the only number format the shell needs.
trait ParseU64 {
    fn parse_u64(&self) -> Option<u64>;
}

impl ParseU64 for str {
    fn parse_u64(&self) -> Option<u64> {
        if self.is_empty() || !self.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut n: u64 = 0;
        for b in self.bytes() {
            n = n.checked_mul(10)?.checked_add((b - b'0') as u64)?;
        }
        Some(n)
    }
}
