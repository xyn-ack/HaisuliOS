// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS ABI.
//!
//! System call numbers, error numbers and IPC protocols shared by the kernel and all user programs.
//!
//! Sharing one crate means the two sides cannot drift apart. System calls use the `syscall`
//! instruction: `rax` holds the number, the arguments are in `rdi rsi rdx r10 r8 r9` and the result
//! comes back in `rax` (a negative errno on failure). File and memory calls use the Linux x86_64
//! numbers; HaisuliOS-native calls start at 0x1000. POSIX processes are not system calls: they are
//! the process server's protocol, [`proc`].

#![no_std]

/// System call numbers.
///
/// The kernel implements mechanisms only. POSIX process management (`fork`, `exit`, `wait4`,
/// `getpid`, process groups, signals...) is not a system call: the process server implements it,
/// and programs reach it through the [`proc`] protocol.
pub mod nr {
    // Files and memory, with Linux x86_64 numbers.

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
    /// `()`: lets the other ready tasks run.
    pub const SCHED_YIELD: u64 = 24;
    /// `(fd)`: duplicates `fd` onto the lowest free descriptor.
    pub const DUP: u64 = 32;
    /// `(fd, newfd)`: makes `newfd` a copy of `fd`, closing it first if needed.
    pub const DUP2: u64 = 33;
    /// `()`: returns from an upcall (a signal handler), restoring the interrupted context.
    ///
    /// Only an upcall's restorer may call it, with the stack as the kernel left it (pointing at
    /// the frame's `ucontext`; see [`TASK_UPCALL`]).
    pub const RT_SIGRETURN: u64 = 15;
    /// `(fd, cmd, arg)`: only `F_DUPFD` (duplicate onto the lowest free
    /// descriptor that is at least `arg`).
    pub const FCNTL: u64 = 72;
    /// `(from, num, turn_on)`: grants (or revokes) I/O ports `from..from + num`.
    ///
    /// Only privileged tasks may use it, for ports below [`IO_PORTS`](super::IO_PORTS).
    pub const IOPERM: u64 = 173;
    /// `(fd, buf, len)`: reads directory entries (Linux `struct linux_dirent64`).
    ///
    /// Returns the number of bytes filled, 0 at the end of the directory.
    pub const GETDENTS64: u64 = 217;

    // HaisuliOS-native microkernel calls.

    /// `(dest, word0, word1)`: sends an asynchronous, non-blocking notification to task `dest`.
    pub const IPC_SEND: u64 = 0x1000;
    /// `(*mut [u64; 3])`: waits for a message and fills in `[sender, word0, word1]`.
    pub const IPC_RECV: u64 = 0x1001;
    /// `(dest, op, buf, len)`: sends a synchronous request to task `dest`.
    ///
    /// Blocks until the server replies and returns the server's reply status.
    /// The server may read and write `buf[..len]` with [`IPC_READ`] /
    /// [`IPC_WRITE`] while the call is pending.
    pub const IPC_CALL: u64 = 0x1002;
    /// `(client, status)`: completes a received call.
    ///
    /// The client's [`IPC_CALL`] returns `status`.
    pub const IPC_REPLY: u64 = 0x1003;
    /// `(client, offset, dst, len)`: copies from the client's call buffer.
    ///
    /// Fails with `-EACCES` if the call does not let the server read the buffer
    /// (see [`console`](super::console)).
    pub const IPC_READ: u64 = 0x1004;
    /// `(client, offset, src, len)`: copies into the client's call buffer.
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
    /// `(service_id)`: returns the task id of a registered service, or `-ESRCH`.
    pub const SVC_LOOKUP: u64 = 0x1009;
    /// `(buf, len)`: writes to the kernel debug log.
    ///
    /// It never blocks, so servers and panic messages, which must not depend on
    /// the console, can always use it.
    pub const DEBUG_LOG: u64 = 0x100A;
    /// `(service_id)`: like [`SVC_LOOKUP`], but waits until the service is registered.
    ///
    /// Boot modules start in any order, so a client may ask before its server is up.
    pub const SVC_WAIT: u64 = 0x100D;
    /// `()`: returns the caller's task id.
    pub const TASK_SELF: u64 = 0x100E;
    /// `()`: ends the calling task.
    ///
    /// The task manager is told with [`KMSG_TASK_EXIT`](super::KMSG_TASK_EXIT). Programs end
    /// through the process server instead (its `OP_EXIT`); this is for when there is none.
    pub const TASK_EXIT: u64 = 0x1010;
    /// `(tid)`: creates a copy of task `tid`, which must be blocked in a call to the caller.
    ///
    /// The copy gets a copy of the address space, the registers and the file descriptors, but no
    /// privileges; its pending call returns 0. Returns the new task's id. Task manager only.
    pub const TASK_FORK: u64 = 0x1011;
    /// `(tid)`: ends task `tid` at once. Task manager only.
    pub const TASK_KILL: u64 = 0x1012;
    /// `(tid)`: stops scheduling task `tid`, wherever it is. Task manager only.
    pub const TASK_STOP: u64 = 0x1013;
    /// `(tid)`: schedules a stopped task again. Task manager only.
    pub const TASK_CONT: u64 = 0x1014;
    /// `(tid, *const [u64; upcall::WORDS])`: makes task `tid` enter a handler.
    ///
    /// See [`upcall`](super::upcall). Fails with `-EBUSY` while an earlier upcall for the task
    /// has not been entered yet. Task manager only.
    pub const TASK_UPCALL: u64 = 0x1015;
    /// `(tid, buf, len)`: fills `buf` with a [`TaskInfo`](super::TaskInfo) about task `tid`.
    pub const TASK_INFO: u64 = 0x1016;
    /// `()`: powers the machine off. Privileged tasks only.
    pub const POWER_OFF: u64 = 0x1017;
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

/// Memory mapping: `MMAP` protection bits (Linux values).
pub mod mman {
    /// Pages may be read (always true for a mapping).
    pub const PROT_READ: u64 = 1;
    /// Pages may be written.
    pub const PROT_WRITE: u64 = 2;
    /// Pages may be executed.
    pub const PROT_EXEC: u64 = 4;
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
/// (no task has id 0, so such messages cannot be forged).
pub const PID_KERNEL: u64 = 0;

/// Kernel notification: `[PID_KERNEL, KMSG_IRQ, irq_line]`.
///
/// The line stays masked until the handler calls [`nr::IRQ_ACK`].
pub const KMSG_IRQ: u64 = 1;

/// Kernel notification to the task manager: `[PID_KERNEL, KMSG_TASK_EXIT, tid]`.
///
/// Task `tid` ended itself with [`nr::TASK_EXIT`]; it is gone. The kernel does not reuse the id
/// before the task manager has received this.
pub const KMSG_TASK_EXIT: u64 = 2;

/// Kernel notification to the task manager: `[PID_KERNEL, KMSG_TASK_FAULT, tid]`.
///
/// Task `tid` caused a CPU exception (or an upcall frame could not be used), so the kernel ended
/// it; it is gone. As for [`KMSG_TASK_EXIT`], the id is not reused before this is received.
pub const KMSG_TASK_FAULT: u64 = 3;

/// The first address above user space.
///
/// Code and stacks of user tasks, and so upcall handlers, must be below it.
pub const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Upcalls: the kernel mechanism under POSIX signal handlers.
///
/// [`nr::TASK_UPCALL`] takes [`WORDS`] words: `[handler, restorer, arg, sender, mask, flags]`.
/// When the task next returns to user mode, the kernel pushes a Linux x86_64 signal frame
/// (`struct rt_sigframe`) below its stack pointer and enters `handler(arg, &siginfo, &ucontext)`,
/// returning to `restorer`. `siginfo.si_signo` is `arg` and `si_pid` is `sender`. The kernel does
/// not interpret `mask`: it stores it as the frame's `uc_sigmask`, for the restorer to hand back
/// to whoever manages signal masks. The restorer ends with [`nr::RT_SIGRETURN`], which restores
/// the registers saved in the frame.
///
/// Posting an upcall also ends an interruptible wait of the task (`ipc_recv`, `svc_wait` or a
/// terminal `read`) with `-EINTR`.
pub mod upcall {
    /// Words in an upcall request.
    pub const WORDS: usize = 6;
    /// Flag: if the system call the task returns from fails with `-EINTR`, make it again after
    /// the handler (POSIX `SA_RESTART`).
    pub const RESTART: u64 = 1;
    /// Where `uc_sigmask` (the `mask` of the request) is, counted from the `ucontext`.
    ///
    /// When the handler returns to the restorer, the stack pointer points at the `ucontext`.
    pub const UC_SIGMASK: usize = 296;
}

/// What a task is doing, as [`TaskInfo::state`] reports it.
pub mod task_state {
    /// Running, or ready to run.
    pub const READY: u64 = 1;
    /// Blocked in the kernel: waiting for a message, a reply or a service.
    pub const BLOCKED: u64 = 2;
    /// Stopped with [`TASK_STOP`](super::nr::TASK_STOP).
    pub const STOPPED: u64 = 3;
}

/// What [`nr::TASK_INFO`] reports about a task.
///
/// It is serialised as [`TASK_INFO_LEN`] bytes, so that neither side needs unsafe code to move it
/// across the system call boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskInfo {
    /// One of [`task_state`].
    pub state: u64,
    /// Whether an upcall waits to be entered: the task has not returned to user mode since the
    /// upcall was requested.
    pub upcall: bool,
    /// The program name, NUL-padded.
    pub name: [u8; PROC_NAME_LEN],
}

/// Serialised size of a [`TaskInfo`].
pub const TASK_INFO_LEN: usize = 2 * 8 + PROC_NAME_LEN;

impl TaskInfo {
    /// Serialises the entry.
    pub fn to_bytes(&self) -> [u8; TASK_INFO_LEN] {
        let mut b = [0u8; TASK_INFO_LEN];
        b[..8].copy_from_slice(&self.state.to_ne_bytes());
        b[8..16].copy_from_slice(&(self.upcall as u64).to_ne_bytes());
        b[16..].copy_from_slice(&self.name);
        b
    }

    /// Parses an entry written by [`to_bytes`](Self::to_bytes).
    pub fn from_bytes(b: &[u8; TASK_INFO_LEN]) -> TaskInfo {
        let word = |i: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&b[i * 8..i * 8 + 8]);
            u64::from_ne_bytes(w)
        };
        let mut name = [0u8; PROC_NAME_LEN];
        name.copy_from_slice(&b[16..]);
        TaskInfo {
            state: word(0),
            upcall: word(1) != 0,
            name,
        }
    }
}

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

/// How a process ended: what its POSIX wait status (`wait4`'s `*status`) encodes.
///
/// The encoding: the low 7 bits hold the signal that killed the process, 0
/// if it exited; after an exit, bits 8 to 15 hold its exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Termination {
    /// It called `exit`; only the low 8 bits of the code are kept.
    Exited(u8),
    /// A signal killed it.
    Signalled(u64),
}

impl Termination {
    /// The wait-status bits holding the signal.
    const SIGNAL_MASK: i32 = 0x7F;
    /// Where the exit code starts in the wait status.
    const CODE_SHIFT: u32 = 8;
    /// A shell reports a process killed by signal n as status 128 + n (POSIX).
    pub const SHELL_SIGNAL_BASE: i32 = 128;

    /// The ending of a process that called `exit(code)`.
    pub const fn exit(code: i32) -> Termination {
        Termination::Exited(code as u8)
    }

    /// The POSIX wait status.
    pub const fn wait_status(self) -> i32 {
        match self {
            Termination::Exited(code) => (code as i32) << Self::CODE_SHIFT,
            Termination::Signalled(sig) => sig as i32 & Self::SIGNAL_MASK,
        }
    }

    /// Decodes a POSIX wait status.
    pub const fn from_wait_status(status: i32) -> Termination {
        match status & Self::SIGNAL_MASK {
            0 => Termination::Exited((status >> Self::CODE_SHIFT) as u8),
            sig => Termination::Signalled(sig as u64),
        }
    }

    /// The status a shell reports in `$?`: the exit code, or 128 + n for a
    /// process killed by signal n.
    pub const fn shell_status(self) -> i32 {
        match self {
            Termination::Exited(code) => code as i32,
            Termination::Signalled(sig) => Self::SHELL_SIGNAL_BASE + sig as i32,
        }
    }
}

/// What a process in a [`ProcInfo`] is doing.
pub mod proc_state {
    /// Runnable or running (`ps` shows `R`).
    pub const RUNNING: u64 = 1;
    /// Blocked, e.g. in `pause`, `wait` or IPC (`ps` shows `S`).
    pub const SLEEPING: u64 = 2;
    /// Stopped by a signal (`ps` shows `T`).
    pub const STOPPED: u64 = 3;
    /// Exited, waiting to be reaped (`ps` shows `Z`).
    pub const ZOMBIE: u64 = 4;
}

/// One process in the listing that the process server's [`proc::OP_LIST`] returns.
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

/// The 16550 UART of the serial console: registers and their bits.
///
/// Shared by the kernel's debug log and the user-space console driver, which
/// program the same device (Linux likewise exports `serial_reg.h` to user
/// space). Register values are written as `bits(&[...])` of their flags.
pub mod uart {
    /// The base I/O port of COM1.
    pub const COM1: u16 = 0x3F8;
    /// I/O ports of one UART, from its base.
    pub const PORTS: u16 = 8;
    /// The IRQ line of COM1.
    pub const COM1_IRQ: u64 = 4;
    /// The divisor for 38400 baud (the UART's clock is 115200 Hz).
    pub const DIVISOR_38400: u16 = 3;

    /// The UART's registers, by offset from the base port.
    #[derive(Clone, Copy, PartialEq, Eq)]
    #[repr(u16)]
    pub enum Register {
        /// Receive buffer (read) and transmit holding register (write).
        Data = 0,
        /// Which events raise an interrupt (see [`InterruptEnable`]).
        InterruptEnable = 1,
        /// FIFO control (write only; see [`FifoControl`]).
        FifoControl = 2,
        /// Line control: the word format, and [`LineControl::DivisorLatch`].
        LineControl = 3,
        /// Modem control (see [`ModemControl`]).
        ModemControl = 4,
        /// Line status (see [`LineStatus`]).
        LineStatus = 5,
    }

    impl Register {
        /// With [`LineControl::DivisorLatch`] set: the divisor's low byte.
        pub const DIVISOR_LOW: Register = Register::Data;
        /// With [`LineControl::DivisorLatch`] set: the divisor's high byte.
        pub const DIVISOR_HIGH: Register = Register::InterruptEnable;
    }

    macro_rules! flags {
        ($(#[$meta:meta])* enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident = $value:expr,)*
        }) => {
            $(#[$meta])*
            #[derive(Clone, Copy, PartialEq, Eq)]
            #[repr(u8)]
            pub enum $name {
                $($(#[$variant_meta])* $variant = $value,)*
            }

            impl $name {
                /// The register value with exactly `flags` set.
                pub const fn bits(flags: &[$name]) -> u8 {
                    let mut bits = 0;
                    let mut i = 0;
                    while i < flags.len() {
                        bits |= flags[i] as u8;
                        i += 1;
                    }
                    bits
                }
            }
        };
    }

    flags! {
        /// [`Register::InterruptEnable`] bits.
        enum InterruptEnable {
            /// A byte has been received.
            ReceivedData = 0x01,
        }
    }

    flags! {
        /// [`Register::FifoControl`] bits.
        enum FifoControl {
            /// Use the FIFOs.
            Enable = 0x01,
            /// Empty the receive FIFO.
            ClearReceive = 0x02,
            /// Empty the transmit FIFO.
            ClearTransmit = 0x04,
            /// Interrupt when 14 bytes are waiting (the largest trigger level).
            Trigger14 = 0xC0,
        }
    }

    flags! {
        /// [`Register::LineControl`] bits.
        enum LineControl {
            /// 8 data bits (with no parity and one stop bit: "8N1").
            Word8 = 0x03,
            /// DLAB: registers 0 and 1 hold the baud-rate divisor.
            DivisorLatch = 0x80,
        }
    }

    flags! {
        /// [`Register::ModemControl`] bits.
        enum ModemControl {
            /// DTR: data terminal ready.
            DataTerminalReady = 0x01,
            /// RTS: request to send.
            RequestToSend = 0x02,
            /// OUT2, which on a PC connects the UART's interrupt to the PIC.
            Out2 = 0x08,
        }
    }

    flags! {
        /// [`Register::LineStatus`] bits.
        enum LineStatus {
            /// A received byte is waiting in [`Register::Data`].
            DataReady = 0x01,
            /// The transmit holding register can take another byte.
            TransmitterEmpty = 0x20,
        }
    }
}

/// Number of legacy I/O ports (0..IO_PORTS) that `IOPERM` can grant.
pub const IO_PORTS: u64 = 0x400;

/// Well-known service identifiers for `SVC_REGISTER` / `SVC_LOOKUP`.
pub mod svc {
    /// Console / TTY server. `read(0)` and `write(1|2)` are routed to it.
    pub const CONSOLE: u64 = 1;
    /// The process server, which implements POSIX processes (see [`proc`](super::proc)).
    ///
    /// The kernel makes the task registered under this id its *task manager*: the only task that
    /// may use the task control calls (`TASK_FORK`, `TASK_KILL`, ...), and the one it tells when
    /// a task ends by itself.
    pub const PROC: u64 = 2;
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

/// Process server protocol (`IPC_CALL` operations to the [`svc::PROC`] service).
///
/// POSIX process management lives in this user-space server, not in the kernel: process ids,
/// parents and children, `fork`, exit statuses and `wait4`, zombies, process groups and sessions,
/// and signals. The user runtime turns the POSIX calls into these requests.
///
/// The call buffer of every request is an array of native-endian `u64` words, whose meaning is
/// listed with each operation; words marked *out* are written by the server. The reply is the
/// POSIX call's result, or a negative errno. The calls take the same arguments as their Linux
/// counterparts and follow the same rules; pids are the server's, not kernel task ids.
pub mod proc {
    /// `[]`: the caller's pid.
    pub const OP_GETPID: u64 = 1;
    /// `[]`: the pid of the caller's parent (0 for boot modules).
    pub const OP_GETPPID: u64 = 2;
    /// `[]`: creates a child; replies with its pid, and the child's call returns 0.
    pub const OP_FORK: u64 = 3;
    /// `[code]`: terminates the caller with exit status `code`; there is no reply.
    pub const OP_EXIT: u64 = 4;
    /// `[pid, options, status (out)]`: waits for a child, as `wait4`.
    ///
    /// `pid` > 0 waits for that child, -1 for any, 0 for the caller's process group and < -1
    /// for process group `-pid`; `options` takes [`WNOHANG`](super::WNOHANG). Replies with the
    /// child's pid (0 if `WNOHANG` found none) and stores its POSIX wait status in `status`.
    pub const OP_WAIT: u64 = 5;
    /// `[pid, sig]`: sends a signal, as `kill` (signal 0 checks that the target exists).
    pub const OP_KILL: u64 = 6;
    /// `[pgid, sig]`: sends `sig` to process group `pgid`, even group 1.
    ///
    /// What the terminal needs (Linux's tty driver calls the kernel's `kill_pgrp()` for it):
    /// `kill(-pgid)` cannot address process group 1, init's, because `kill(-1)` signals every
    /// process.
    pub const OP_KILL_PGRP: u64 = 7;
    /// `[]`: sleeps until a signal handler is about to run (`pause`).
    ///
    /// Then replies 0, which the runtime reports as `pause`'s `EINTR`. Not `-EINTR`: the kernel
    /// restarts a call that fails with `EINTR` after a `SA_RESTART` handler, and POSIX never
    /// restarts `pause`.
    pub const OP_PAUSE: u64 = 8;
    /// `[sig, set, handler, flags, restorer, mask, old handler (out), old flags (out),
    /// old restorer (out), old mask (out)]`: `rt_sigaction`.
    ///
    /// The new disposition (a Linux `struct sigaction`) only applies if `set` is 1. A handler
    /// needs `SA_RESTORER`: the restorer hands the frame's mask back with [`OP_SIGRETURN`], then
    /// makes the kernel's `RT_SIGRETURN` call.
    pub const OP_SIGACTION: u64 = 9;
    /// `[how, set?, set, old (out)]`: `rt_sigprocmask`; `set` only applies if `set?` is 1.
    pub const OP_SIGPROCMASK: u64 = 10;
    /// `[mask]`: a handler has returned; restores the signal mask saved in its frame.
    pub const OP_SIGRETURN: u64 = 11;
    /// `[pid, pgid]`: `setpgid`.
    pub const OP_SETPGID: u64 = 12;
    /// `[pid]`: `getpgid`.
    pub const OP_GETPGID: u64 = 13;
    /// `[]`: `setsid`.
    pub const OP_SETSID: u64 = 14;
    /// `[pid]`: `getsid`.
    pub const OP_GETSID: u64 = 15;
    /// A buffer of [`ProcInfo`](super::ProcInfo) entries (out): lists the processes.
    ///
    /// Replies with the number of entries written; `ps` is built on it.
    pub const OP_LIST: u64 = 16;
}
