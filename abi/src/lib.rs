// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS ABI.
//!
//! System call numbers, error numbers and IPC protocols shared by the kernel and all user programs.
//!
//! Sharing one crate means the two sides cannot drift apart. System calls use the `syscall`
//! instruction: `rax` holds the number, the arguments are in `rdi rsi rdx r10 r8 r9` and the result
//! comes back in `rax` (a negative errno on failure). POSIX calls use the Linux x86_64 numbers;
//! HaisuliOS-native calls start at 0x1000.

#![no_std]

/// System call numbers.
pub mod nr {
    // The POSIX subset, with Linux x86_64 numbers.

    /// `(fd, buf, len)`: reads from a file, or from the terminal (the console server).
    pub const READ: u64 = 0;
    /// `(fd, buf, len)`: writes to a file, or to the terminal (the console server).
    pub const WRITE: u64 = 1;
    /// `(path, flags, mode)`: opens a file and returns the lowest free descriptor.
    ///
    /// `path` is NUL-terminated; `flags` take the [`O_*`](super::fcntl) values.
    pub const OPEN: u64 = 2;
    /// `(fd)`: closes a file descriptor.
    pub const CLOSE: u64 = 3;
    /// `(addr, len, prot, flags, fd, offset)`: maps anonymous memory; the kernel picks the address.
    pub const MMAP: u64 = 9;
    /// `()`: lets the other ready processes run.
    pub const SCHED_YIELD: u64 = 24;
    /// `(fd)`: duplicates `fd` onto the lowest free descriptor.
    pub const DUP: u64 = 32;
    /// `(fd, newfd)`: makes `newfd` a copy of `fd`, closing it first if needed.
    pub const DUP2: u64 = 33;
    /// `()`: returns the caller's process id.
    pub const GETPID: u64 = 39;
    /// `()`: creates a copy of the caller, which returns 0 in the child.
    pub const FORK: u64 = 57;
    /// `(code)`: terminates the caller with exit status `code`.
    pub const EXIT: u64 = 60;
    /// `(pid, status, options, rusage)`: waits for a child to exit.
    ///
    /// `pid` > 0 waits for that child, -1 for any, 0 for the caller's process
    /// group and < -1 for process group `-pid`. `options` takes
    /// [`WNOHANG`](super::WNOHANG).
    pub const WAIT4: u64 = 61;
    /// `()`: returns the process id of the caller's parent.
    pub const GETPPID: u64 = 110;
    /// `(sig, act, oldact, sigsetsize)`: sets the disposition of signal `sig`.
    ///
    /// `act` and `oldact` are Linux `struct sigaction`s. A handler needs
    /// `SA_RESTORER` and a restorer that calls [`RT_SIGRETURN`].
    pub const RT_SIGACTION: u64 = 13;
    /// `(how, set, oldset, sigsetsize)`: changes the set of blocked signals.
    pub const RT_SIGPROCMASK: u64 = 14;
    /// `()`: returns from a signal handler, restoring the interrupted context.
    ///
    /// Only a handler's restorer may call it, with the stack as the kernel
    /// left it (pointing at the signal frame's `ucontext`).
    pub const RT_SIGRETURN: u64 = 15;
    /// `()`: sleeps until a signal terminates the process or a handler runs.
    pub const PAUSE: u64 = 34;
    /// `(pid, sig)`: sends signal `sig`, or checks for the target if `sig` is 0.
    ///
    /// `pid` > 0 signals that process, 0 the caller's process group, < -1
    /// process group `-pid` and -1 every process the caller may signal.
    pub const KILL: u64 = 62;
    /// `(pid, pgid)`: puts process `pid` (0 = the caller) into process group `pgid`.
    pub const SETPGID: u64 = 109;
    /// `()`: makes the caller the leader of a new session and process group.
    pub const SETSID: u64 = 112;
    /// `(pid)`: returns the process group of `pid` (0 = the caller).
    pub const GETPGID: u64 = 121;
    /// `(pid)`: returns the session of `pid` (0 = the caller).
    pub const GETSID: u64 = 124;
    /// `(fd, cmd, arg)`: only `F_DUPFD` (duplicate onto the lowest free
    /// descriptor that is at least `arg`).
    pub const FCNTL: u64 = 72;
    /// `(from, num, turn_on)`: grants (or revokes) I/O ports `from..from + num`.
    ///
    /// Only privileged processes may use it, for ports below [`IO_PORTS`](super::IO_PORTS).
    pub const IOPERM: u64 = 173;
    /// `(fd, buf, len)`: reads directory entries (Linux `struct linux_dirent64`).
    ///
    /// Returns the number of bytes filled, 0 at the end of the directory.
    pub const GETDENTS64: u64 = 217;

    // HaisuliOS-native microkernel calls.

    /// `(dest_pid, word0, word1)`: sends an asynchronous, non-blocking notification.
    pub const IPC_SEND: u64 = 0x1000;
    /// `(*mut [u64; 3])`: waits for a message and fills in `[sender, word0, word1]`.
    pub const IPC_RECV: u64 = 0x1001;
    /// `(dest_pid, op, buf, len)`: sends a synchronous request.
    ///
    /// Blocks until the server replies and returns the server's reply status.
    /// The server may read and write `buf[..len]` with [`IPC_READ`] /
    /// [`IPC_WRITE`] while the call is pending.
    pub const IPC_CALL: u64 = 0x1002;
    /// `(client_pid, status)`: completes a received call.
    ///
    /// The client's [`IPC_CALL`] returns `status`.
    pub const IPC_REPLY: u64 = 0x1003;
    /// `(client_pid, offset, dst, len)`: copies from the client's call buffer.
    ///
    /// Fails with `-EACCES` if the call does not let the server read the buffer
    /// (see [`console`](super::console)).
    pub const IPC_READ: u64 = 0x1004;
    /// `(client_pid, offset, src, len)`: copies into the client's call buffer.
    ///
    /// Fails with `-EACCES` if the call does not let the server write the buffer
    /// (see [`console`](super::console)).
    pub const IPC_WRITE: u64 = 0x1005;
    /// `(irq)`: routes hardware IRQ line `irq` to the (privileged) caller.
    pub const IRQ_BIND: u64 = 0x1006;
    /// `(irq)`: re-enables a bound IRQ line after its device has been serviced.
    pub const IRQ_ACK: u64 = 0x1007;
    /// `(service_id)`: registers the (privileged) caller as a well-known service.
    pub const SVC_REGISTER: u64 = 0x1008;
    /// `(service_id)`: returns the pid of a registered service, or `-ESRCH`.
    pub const SVC_LOOKUP: u64 = 0x1009;
    /// `(buf, len)`: fills `buf` with one [`ProcInfo`](super::ProcInfo) per process.
    ///
    /// Returns the number of processes reported; `ps` is built on it.
    pub const PROC_LIST: u64 = 0x100B;
    /// `(pgid, sig)`: sends signal `sig` to every process in group `pgid`.
    ///
    /// What the terminal needs (Linux's tty driver calls the kernel's
    /// `kill_pgrp()` for it): `kill(-pgid)` cannot address process group 1,
    /// init's, because `kill(-1)` signals every process.
    pub const KILL_PGRP: u64 = 0x100C;
    /// `(buf, len)`: writes to the kernel debug log.
    ///
    /// It never blocks, so servers and panic messages, which must not depend on
    /// the console, can always use it.
    pub const DEBUG_LOG: u64 = 0x100A;
}

/// Error numbers (Linux values), returned negated.
pub mod errno {
    /// Operation not permitted (e.g. a privileged call from an ordinary process).
    pub const EPERM: i64 = 1;
    /// No such file or directory.
    pub const ENOENT: i64 = 2;
    /// No such process (or no such registered service).
    pub const ESRCH: i64 = 3;
    /// Interrupted by a signal (e.g. a console read abandoned with Ctrl-C).
    pub const EINTR: i64 = 4;
    /// Input/output error.
    pub const EIO: i64 = 5;
    /// Bad file descriptor.
    pub const EBADF: i64 = 9;
    /// No child processes.
    pub const ECHILD: i64 = 10;
    /// Resource temporarily unavailable (e.g. a full mailbox).
    pub const EAGAIN: i64 = 11;
    /// Out of memory.
    pub const ENOMEM: i64 = 12;
    /// Permission denied (e.g. a transfer the pending call does not allow).
    pub const EACCES: i64 = 13;
    /// Bad address: memory that is not mapped, not user memory or not writable.
    pub const EFAULT: i64 = 14;
    /// Resource busy (e.g. an IRQ line or service that is already taken).
    pub const EBUSY: i64 = 16;
    /// File exists (`O_CREAT | O_EXCL`).
    pub const EEXIST: i64 = 17;
    /// Not a directory.
    pub const ENOTDIR: i64 = 20;
    /// Is a directory.
    pub const EISDIR: i64 = 21;
    /// Invalid argument.
    pub const EINVAL: i64 = 22;
    /// Too many open files in the system.
    pub const ENFILE: i64 = 23;
    /// Too many open files in the process.
    pub const EMFILE: i64 = 24;
    /// File too large.
    pub const EFBIG: i64 = 27;
    /// No space left on the device (the file system is full).
    pub const ENOSPC: i64 = 28;
    /// Broken pipe: the server of a pending call has exited.
    pub const EPIPE: i64 = 32;
    /// A call would deadlock (e.g. a process calling itself).
    pub const EDEADLK: i64 = 35;
    /// Unknown system call or operation.
    pub const ENOSYS: i64 = 38;
    /// File name too long.
    pub const ENAMETOOLONG: i64 = 36;

    /// The POSIX description of an error number, as `strerror` gives it.
    pub fn describe(errno: i64) -> &'static str {
        match errno {
            EPERM => "Operation not permitted",
            ENOENT => "No such file or directory",
            ESRCH => "No such process",
            EINTR => "Interrupted system call",
            EIO => "Input/output error",
            EBADF => "Bad file descriptor",
            ECHILD => "No child processes",
            EAGAIN => "Resource temporarily unavailable",
            ENOMEM => "Cannot allocate memory",
            EACCES => "Permission denied",
            EFAULT => "Bad address",
            EBUSY => "Device or resource busy",
            EEXIST => "File exists",
            ENOTDIR => "Not a directory",
            EISDIR => "Is a directory",
            EINVAL => "Invalid argument",
            ENFILE => "Too many open files in system",
            EMFILE => "Too many open files",
            EFBIG => "File too large",
            ENOSPC => "No space left on device",
            EPIPE => "Broken pipe",
            EDEADLK => "Resource deadlock avoided",
            ENAMETOOLONG => "File name too long",
            ENOSYS => "Function not implemented",
            _ => "Unknown error",
        }
    }
}

/// File access: `OPEN` flags, `FCNTL` commands and directory entries (Linux values).
pub mod fcntl {
    /// Open for reading only.
    pub const O_RDONLY: u64 = 0;
    /// Open for writing only.
    pub const O_WRONLY: u64 = 1;
    /// Open for reading and writing.
    pub const O_RDWR: u64 = 2;
    /// The access mode bits of the flags.
    pub const O_ACCMODE: u64 = 3;
    /// Create the file if it does not exist.
    pub const O_CREAT: u64 = 0o100;
    /// With `O_CREAT`: fail with `EEXIST` if the file exists.
    pub const O_EXCL: u64 = 0o200;
    /// Truncate the file to length 0 (if opened for writing).
    pub const O_TRUNC: u64 = 0o1000;
    /// Every write goes to the end of the file.
    pub const O_APPEND: u64 = 0o2000;
    /// Fail with `ENOTDIR` unless the path is a directory.
    pub const O_DIRECTORY: u64 = 0o200000;

    /// `FCNTL`: duplicate onto the lowest free descriptor that is at least `arg`.
    pub const F_DUPFD: u64 = 0;

    /// Longest path, in bytes (without the terminating NUL).
    pub const PATH_MAX: usize = 127;
    /// Most descriptors a process may have open (`0..OPEN_MAX`).
    pub const OPEN_MAX: u64 = 16;

    /// `d_type` of a directory in a directory entry.
    pub const DT_DIR: u8 = 4;
    /// `d_type` of a regular file in a directory entry.
    pub const DT_REG: u8 = 8;
    /// Size of the fixed part of a `struct linux_dirent64`, before `d_name`:
    /// `d_ino` (8), `d_off` (8), `d_reclen` (2) and `d_type` (1).
    pub const DIRENT_HEADER: usize = 19;
}

/// Messages and calls whose sender is `PID_KERNEL` come from the kernel itself
/// (no user process can have pid 0, so such messages cannot be forged).
pub const PID_KERNEL: u64 = 0;

/// Kernel notification: `[PID_KERNEL, KMSG_IRQ, irq_line]`.
///
/// The line stays masked until the handler calls [`nr::IRQ_ACK`].
pub const KMSG_IRQ: u64 = 1;

/// Signal numbers and dispositions (the Linux x86_64 values).
pub mod signal {
    /// Hangup.
    pub const SIGHUP: u64 = 1;
    /// Terminal interrupt (Ctrl-C).
    pub const SIGINT: u64 = 2;
    /// Terminal quit.
    pub const SIGQUIT: u64 = 3;
    /// Kill: cannot be caught, ignored or blocked while stopped.
    pub const SIGKILL: u64 = 9;
    /// User-defined signal 1.
    pub const SIGUSR1: u64 = 10;
    /// Invalid memory reference (raised by the kernel's fault handler).
    pub const SIGSEGV: u64 = 11;
    /// User-defined signal 2.
    pub const SIGUSR2: u64 = 12;
    /// Broken pipe.
    pub const SIGPIPE: u64 = 13;
    /// Termination request: the default signal of `kill`.
    pub const SIGTERM: u64 = 15;
    /// Child stopped or terminated (ignored by default).
    pub const SIGCHLD: u64 = 17;
    /// Continue a stopped process.
    pub const SIGCONT: u64 = 18;
    /// Stop: cannot be caught or ignored.
    pub const SIGSTOP: u64 = 19;
    /// Terminal stop (Ctrl-Z).
    pub const SIGTSTP: u64 = 20;
    /// Background read from the terminal.
    pub const SIGTTIN: u64 = 21;
    /// Background write to the terminal.
    pub const SIGTTOU: u64 = 22;
    /// One more than the highest signal number.
    pub const NSIG: u64 = 32;

    /// Default disposition (the `act.handler` value of `RT_SIGACTION`).
    pub const SIG_DFL: u64 = 0;
    /// Ignore the signal (the `act.handler` value of `RT_SIGACTION`).
    pub const SIG_IGN: u64 = 1;

    /// `sa_flags`: the handler takes `(sig, &siginfo, &ucontext)`.
    ///
    /// The kernel always passes all three, so this flag changes nothing.
    pub const SA_SIGINFO: u64 = 0x0000_0004;
    /// `sa_flags`: `sa_restorer` is set; required for a handler.
    pub const SA_RESTORER: u64 = 0x0400_0000;
    /// `sa_flags`: restart an interrupted `read`, `wait4` or `ipc_recv`
    /// after the handler, instead of failing it with `EINTR`.
    pub const SA_RESTART: u64 = 0x1000_0000;
    /// `sa_flags`: do not block the signal while its handler runs.
    pub const SA_NODEFER: u64 = 0x4000_0000;
    /// `sa_flags`: reset the disposition to `SIG_DFL` when the handler runs.
    pub const SA_RESETHAND: u64 = 0x8000_0000;

    /// `RT_SIGPROCMASK`: add `set` to the blocked signals.
    pub const SIG_BLOCK: u64 = 0;
    /// `RT_SIGPROCMASK`: remove `set` from the blocked signals.
    pub const SIG_UNBLOCK: u64 = 1;
    /// `RT_SIGPROCMASK`: make `set` the blocked signals.
    pub const SIG_SETMASK: u64 = 2;

    /// The bit of signal `sig` in a signal set (`sigset_t`), as Linux lays it out.
    pub const fn bit(sig: u64) -> u64 {
        if sig == 0 || sig >= NSIG {
            0
        } else {
            1 << (sig - 1)
        }
    }

    /// Signal names, without the `SIG` prefix, indexed by signal number.
    ///
    /// An empty name means the number is unassigned.
    pub const NAMES: [&str; NSIG as usize] = [
        "", "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV",
        "USR2", "PIPE", "ALRM", "TERM", "STKFLT", "CHLD", "CONT", "STOP", "TSTP", "TTIN", "TTOU",
        "URG", "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "IO", "PWR", "SYS",
    ];

    /// The signal number called `name` (with or without the `SIG` prefix).
    pub fn by_name(name: &str) -> Option<u64> {
        let name = name.strip_prefix("SIG").unwrap_or(name);
        NAMES
            .iter()
            .position(|&n| !n.is_empty() && n.eq_ignore_ascii_case(name))
            .map(|i| i as u64)
    }
}

/// `wait4` option: report nothing instead of blocking if no child has exited.
pub const WNOHANG: u64 = 1;

/// The POSIX wait status of a process that exited with `code`.
pub const fn wait_status_exited(code: i32) -> i32 {
    (code & 0xFF) << 8
}

/// The POSIX wait status of a process killed by signal `sig`.
pub const fn wait_status_signalled(sig: u64) -> i32 {
    (sig & 0x7F) as i32
}

/// What a process in a [`ProcInfo`] is doing.
pub mod proc_state {
    /// Runnable or running (`ps` shows `R`).
    pub const RUNNING: u64 = 1;
    /// Blocked in the kernel, e.g. in `pause` or IPC (`ps` shows `S`).
    pub const SLEEPING: u64 = 2;
    /// Stopped by a signal (`ps` shows `T`).
    pub const STOPPED: u64 = 3;
    /// Exited, waiting to be reaped (`ps` shows `Z`).
    pub const ZOMBIE: u64 = 4;
}

/// One process in the listing that [`nr::PROC_LIST`] returns.
///
/// It is serialised as [`PROC_INFO_LEN`] bytes, so that neither side needs
/// unsafe code to move it across the system call boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcInfo {
    /// Process id.
    pub pid: u64,
    /// Parent process id (0 for boot modules).
    pub ppid: u64,
    /// Process group id.
    pub pgid: u64,
    /// Session id.
    pub sid: u64,
    /// One of [`proc_state`].
    pub state: u64,
    /// The program name, NUL-padded.
    pub name: [u8; PROC_NAME_LEN],
}

/// Length of a process name in a [`ProcInfo`].
pub const PROC_NAME_LEN: usize = 16;
/// Serialised size of a [`ProcInfo`].
pub const PROC_INFO_LEN: usize = 5 * 8 + PROC_NAME_LEN;

impl ProcInfo {
    /// Serialises the entry.
    pub fn to_bytes(&self) -> [u8; PROC_INFO_LEN] {
        let mut b = [0u8; PROC_INFO_LEN];
        for (i, v) in [self.pid, self.ppid, self.pgid, self.sid, self.state]
            .iter()
            .enumerate()
        {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
        }
        b[40..].copy_from_slice(&self.name);
        b
    }

    /// Parses an entry written by [`to_bytes`](Self::to_bytes).
    pub fn from_bytes(b: &[u8; PROC_INFO_LEN]) -> ProcInfo {
        let word = |i: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&b[i * 8..i * 8 + 8]);
            u64::from_ne_bytes(w)
        };
        let mut name = [0u8; PROC_NAME_LEN];
        name.copy_from_slice(&b[40..]);
        ProcInfo {
            pid: word(0),
            ppid: word(1),
            pgid: word(2),
            sid: word(3),
            state: word(4),
            name,
        }
    }

    /// The program name as text.
    pub fn name(&self) -> &str {
        let n = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(PROC_NAME_LEN);
        core::str::from_utf8(&self.name[..n]).unwrap_or("?")
    }
}

/// Number of legacy I/O ports (0..IO_PORTS) that `IOPERM` can grant.
pub const IO_PORTS: u64 = 0x400;

/// Well-known service identifiers for `SVC_REGISTER` / `SVC_LOOKUP`.
pub mod svc {
    /// Console / TTY server. `read(0)` and `write(1|2)` are routed to it.
    pub const CONSOLE: u64 = 1;
    /// Number of service slots (ids are `1..MAX`).
    pub const MAX: u64 = 8;
}

/// Console server protocol (`IPC_CALL` operations).
///
/// When the kernel forwards `write(1|2)` and `read(0)`, it limits what the
/// server may do with the caller's buffer: for `OP_WRITE` the server can only
/// read it, for `OP_READ` only write it (`IPC_WRITE` / `IPC_READ` fail with
/// `-EACCES` otherwise). Explicit `IPC_CALL`s allow both.
pub mod console {
    /// Writes `buf[..len]` to the terminal.
    ///
    /// The reply is the number of bytes written.
    pub const OP_WRITE: u64 = 1;
    /// Reads one line of input, at most `len` bytes, into `buf`.
    ///
    /// Blocks until a line (or end-of-file, Ctrl-D) is available. The reply is
    /// the number of bytes read (0 at end of file), or `-EINTR` if Ctrl-C
    /// interrupted the line.
    pub const OP_READ: u64 = 2;
    /// Makes the process group in `buf` (a `u64`) the foreground one.
    ///
    /// The terminal sends Ctrl-C's `SIGINT` to that group, and interrupts its
    /// pending reads; this is the `tcsetpgrp()` of HaisuliOS.
    pub const OP_SET_FG_PGRP: u64 = 3;
}
