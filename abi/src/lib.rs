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

    /// `(fd, buf, len)`: reads from `fd` 0, which is served by the console server.
    pub const READ: u64 = 0;
    /// `(fd, buf, len)`: writes to `fd` 1 or 2, which are served by the console server.
    pub const WRITE: u64 = 1;
    /// `(addr, len, prot, flags, fd, offset)`: maps anonymous memory; the kernel picks the address.
    pub const MMAP: u64 = 9;
    /// `(sig, act, oldact, sigsetsize)`: examines and changes the action for signal `sig`.
    ///
    /// `act` and `oldact` point to a [`SigAction`](super::signal::SigAction) (or are 0);
    /// `sigsetsize` must be 8.
    pub const RT_SIGACTION: u64 = 13;
    /// `(how, set, oldset, sigsetsize)`: examines and changes the blocked signals.
    pub const RT_SIGPROCMASK: u64 = 14;
    /// `()`: returns from a signal handler; only for the handler's restorer.
    pub const RT_SIGRETURN: u64 = 15;
    /// `(fd, request, arg)`: terminal control on `fd` 0, 1 or 2 (see [`tty`](super::tty)).
    pub const IOCTL: u64 = 16;
    /// `()`: lets the other ready processes run.
    pub const SCHED_YIELD: u64 = 24;
    /// `()`: waits until a signal handler has run; always fails with `EINTR`.
    pub const PAUSE: u64 = 34;
    /// `()`: returns the caller's process id.
    pub const GETPID: u64 = 39;
    /// `()`: creates a copy of the caller, which returns 0 in the child.
    pub const FORK: u64 = 57;
    /// `(code)`: terminates the caller with exit status `code`.
    pub const EXIT: u64 = 60;
    /// `(pid, status, options, rusage)`: waits for a child to change state.
    ///
    /// `pid` > 0 selects that child, -1 any child, 0 any child in the caller's process group and
    /// < -1 any child in process group `-pid`. Options are in [`wait`](super::wait); `rusage` is
    /// ignored.
    pub const WAIT4: u64 = 61;
    /// `(pid, sig)`: sends signal `sig` (0 only checks) to a process or process group.
    ///
    /// `pid` > 0 is a process, 0 the caller's process group, -1 every process the caller may
    /// signal except itself and init, and < -1 process group `-pid`.
    pub const KILL: u64 = 62;
    /// `(pid, pgid)`: moves process `pid` (0: the caller) into process group `pgid` (0: `pid`).
    pub const SETPGID: u64 = 109;
    /// `()`: returns the process id of the caller's parent.
    pub const GETPPID: u64 = 110;
    /// `()`: returns the caller's process group id.
    pub const GETPGRP: u64 = 111;
    /// `()`: makes the caller the leader of a new session and process group.
    pub const SETSID: u64 = 112;
    /// `(pid)`: returns the process group id of `pid` (0: the caller).
    pub const GETPGID: u64 = 121;
    /// `(pid)`: returns the session id of `pid` (0: the caller).
    pub const GETSID: u64 = 124;
    /// `(option, arg2, ...)`: process attributes; only [`PR_SET_NAME`](super::PR_SET_NAME) and
    /// [`PR_GET_NAME`](super::PR_GET_NAME).
    pub const PRCTL: u64 = 157;
    /// `(from, num, turn_on)`: grants (or revokes) I/O ports `from..from + num`.
    ///
    /// Only privileged processes may use it, for ports below [`IO_PORTS`](super::IO_PORTS).
    pub const IOPERM: u64 = 173;

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
    /// `(buf, len)`: writes to the kernel debug log.
    ///
    /// It never blocks, so servers and panic messages, which must not depend on
    /// the console, can always use it.
    pub const DEBUG_LOG: u64 = 0x100A;
    /// `(pid, buf)`: describes the process with the lowest pid >= `pid`.
    ///
    /// Fills `buf` with a [`ProcInfo`](super::ProcInfo) record ([`ProcInfo::SIZE`] bytes) and
    /// returns that process's pid, or fails with `-ESRCH` if there is none. Used by `ps`.
    ///
    /// [`ProcInfo::SIZE`]: super::ProcInfo::SIZE
    pub const PROC_INFO: u64 = 0x100B;
    /// `(pgrp, sig)`: sends signal `sig` to every process in process group `pgrp`.
    ///
    /// Like [`KILL`] with `-pgrp`, but it can address process group 1 (init's), for which
    /// `kill(-1, sig)` would mean every process. The console server signals the terminal's
    /// foreground process group with it.
    pub const KILLPG: u64 = 0x100C;
}

/// Error numbers (Linux values), returned negated.
pub mod errno {
    /// Operation not permitted (e.g. a privileged call from an ordinary process).
    pub const EPERM: i64 = 1;
    /// No such process (or no such registered service).
    pub const ESRCH: i64 = 3;
    /// Interrupted system call: a signal arrived while the call was blocked.
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
    /// Not a terminal, or not the caller's controlling terminal.
    pub const ENOTTY: i64 = 25;
    /// Invalid argument.
    pub const EINVAL: i64 = 22;
    /// Broken pipe: the server of a pending call has exited.
    pub const EPIPE: i64 = 32;
    /// A call would deadlock (e.g. a process calling itself).
    pub const EDEADLK: i64 = 35;
    /// Unknown system call or operation.
    pub const ENOSYS: i64 = 38;
}

/// Messages and calls whose sender is `PID_KERNEL` come from the kernel itself
/// (no user process can have pid 0, so such messages cannot be forged).
pub const PID_KERNEL: u64 = 0;

/// Kernel notification: `[PID_KERNEL, KMSG_IRQ, irq_line]`.
///
/// The line stays masked until the handler calls [`nr::IRQ_ACK`].
pub const KMSG_IRQ: u64 = 1;

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
    /// the number of bytes read (0 at end of file).
    pub const OP_READ: u64 = 2;
    /// Makes the process group in `buf` (a 4-byte pid) the terminal's foreground process group.
    ///
    /// The kernel forwards `ioctl(fd, TIOCSPGRP, &pgrp)` as this call. The first session to set
    /// a foreground group gets the terminal as its controlling terminal; the group must belong
    /// to that session, and the caller too.
    pub const OP_SETPGRP: u64 = 3;
    /// Writes the foreground process group (a 4-byte pid; 0 for none) to `buf`.
    ///
    /// The kernel forwards `ioctl(fd, TIOCGPGRP, &pgrp)` as this call.
    pub const OP_GETPGRP: u64 = 4;
}

/// Terminal `ioctl` requests (Linux values).
pub mod tty {
    /// `ioctl(fd, TIOCGPGRP, *mut i32)`: `tcgetpgrp`.
    pub const TIOCGPGRP: u64 = 0x540F;
    /// `ioctl(fd, TIOCSPGRP, *const i32)`: `tcsetpgrp`.
    pub const TIOCSPGRP: u64 = 0x5410;
}

/// `prctl` option: sets the caller's name from a NUL-terminated string (at most 15 bytes used).
pub const PR_SET_NAME: u64 = 15;
/// `prctl` option: copies the caller's name, NUL-terminated, into a 16-byte buffer.
pub const PR_GET_NAME: u64 = 16;

/// Signals (Linux x86_64 numbers).
///
/// Signals 1 to 31 exist; there are no real-time signals yet. A signal set is a `u64` with bit
/// `n - 1` standing for signal `n`.
pub mod signal {
    /// Hangup.
    pub const SIGHUP: u32 = 1;
    /// Terminal interrupt (Ctrl-C).
    pub const SIGINT: u32 = 2;
    /// Terminal quit (`Ctrl-\`).
    pub const SIGQUIT: u32 = 3;
    /// Illegal instruction.
    pub const SIGILL: u32 = 4;
    /// Trace trap.
    pub const SIGTRAP: u32 = 5;
    /// Abort.
    pub const SIGABRT: u32 = 6;
    /// Bus error.
    pub const SIGBUS: u32 = 7;
    /// Arithmetic error.
    pub const SIGFPE: u32 = 8;
    /// Kill (cannot be caught, blocked or ignored).
    pub const SIGKILL: u32 = 9;
    /// User-defined signal 1.
    pub const SIGUSR1: u32 = 10;
    /// Invalid memory reference.
    pub const SIGSEGV: u32 = 11;
    /// User-defined signal 2.
    pub const SIGUSR2: u32 = 12;
    /// Write to a pipe without readers.
    pub const SIGPIPE: u32 = 13;
    /// Alarm clock.
    pub const SIGALRM: u32 = 14;
    /// Termination request.
    pub const SIGTERM: u32 = 15;
    /// Coprocessor stack fault.
    pub const SIGSTKFLT: u32 = 16;
    /// A child stopped, continued or terminated.
    pub const SIGCHLD: u32 = 17;
    /// Continue if stopped.
    pub const SIGCONT: u32 = 18;
    /// Stop (cannot be caught, blocked or ignored).
    pub const SIGSTOP: u32 = 19;
    /// Terminal stop (Ctrl-Z).
    pub const SIGTSTP: u32 = 20;
    /// Background read from the terminal.
    pub const SIGTTIN: u32 = 21;
    /// Background write to the terminal.
    pub const SIGTTOU: u32 = 22;
    /// Urgent data on a socket.
    pub const SIGURG: u32 = 23;
    /// CPU time limit exceeded.
    pub const SIGXCPU: u32 = 24;
    /// File size limit exceeded.
    pub const SIGXFSZ: u32 = 25;
    /// Virtual timer expired.
    pub const SIGVTALRM: u32 = 26;
    /// Profiling timer expired.
    pub const SIGPROF: u32 = 27;
    /// Window size change.
    pub const SIGWINCH: u32 = 28;
    /// I/O possible.
    pub const SIGIO: u32 = 29;
    /// Power failure.
    pub const SIGPWR: u32 = 30;
    /// Bad system call.
    pub const SIGSYS: u32 = 31;
    /// One more than the highest signal number.
    pub const NSIG: u32 = 32;

    /// The signal's name without the `SIG` prefix (e.g. `"TERM"`), for signals 1 to 31.
    pub const NAMES: [&str; NSIG as usize] = [
        "", "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV",
        "USR2", "PIPE", "ALRM", "TERM", "STKFLT", "CHLD", "CONT", "STOP", "TSTP", "TTIN", "TTOU",
        "URG", "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "IO", "PWR", "SYS",
    ];

    /// The bit for signal `sig` in a signal set.
    pub const fn bit(sig: u32) -> u64 {
        1 << (sig - 1)
    }

    /// `SigAction::handler`: the default action.
    pub const SIG_DFL: u64 = 0;
    /// `SigAction::handler`: ignore the signal.
    pub const SIG_IGN: u64 = 1;

    /// `SigAction::flags`: no `SIGCHLD` when a child stops or continues.
    pub const SA_NOCLDSTOP: u64 = 0x1;
    /// `SigAction::flags`: children do not become zombies; `wait4` has nothing to report.
    pub const SA_NOCLDWAIT: u64 = 0x2;
    /// `SigAction::flags`: the handler also gets a `siginfo` pointer (only `si_signo` is set).
    pub const SA_SIGINFO: u64 = 0x4;
    /// `SigAction::flags`: `SigAction::restorer` is valid (it always must be, on x86_64).
    pub const SA_RESTORER: u64 = 0x0400_0000;
    /// `SigAction::flags`: restart an interrupted system call instead of failing with `EINTR`.
    pub const SA_RESTART: u64 = 0x1000_0000;
    /// `SigAction::flags`: do not block the signal while its handler runs.
    pub const SA_NODEFER: u64 = 0x4000_0000;
    /// `SigAction::flags`: reset the action to the default when the handler is called.
    pub const SA_RESETHAND: u64 = 0x8000_0000;

    /// `rt_sigprocmask` `how`: block the signals in `set` too.
    pub const SIG_BLOCK: u64 = 0;
    /// `rt_sigprocmask` `how`: unblock the signals in `set`.
    pub const SIG_UNBLOCK: u64 = 1;
    /// `rt_sigprocmask` `how`: block exactly the signals in `set`.
    pub const SIG_SETMASK: u64 = 2;

    /// The action for a signal (Linux's `struct kernel_sigaction` on x86_64).
    ///
    /// The handler is called as `handler(sig)` (`handler(sig, info, 0)` with `SA_SIGINFO`) and
    /// returns to `restorer`, which must make the `rt_sigreturn` system call.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    #[repr(C)]
    pub struct SigAction {
        /// [`SIG_DFL`], [`SIG_IGN`] or the handler's address.
        pub handler: u64,
        /// `SA_*` flags.
        pub flags: u64,
        /// Where the handler returns to.
        pub restorer: u64,
        /// Signals blocked while the handler runs (besides the signal itself).
        pub mask: u64,
    }
}

/// `wait4` options and wait-status decoding (Linux values).
pub mod wait {
    /// Return 0 at once instead of blocking when no child has changed state.
    pub const WNOHANG: u64 = 1;
    /// Also report children that have stopped.
    pub const WUNTRACED: u64 = 2;
    /// Also report stopped children that have continued.
    pub const WCONTINUED: u64 = 8;

    /// The status of a child that exited normally: `Some(exit code)`.
    pub const fn exit_status(status: i32) -> Option<i32> {
        if status & 0x7F == 0 {
            Some((status >> 8) & 0xFF)
        } else {
            None
        }
    }

    /// The status of a child that a signal terminated: `Some(signal)`.
    pub const fn term_signal(status: i32) -> Option<u32> {
        let sig = status & 0x7F;
        if sig != 0 && sig != 0x7F {
            Some(sig as u32)
        } else {
            None
        }
    }

    /// The status of a child that a signal stopped: `Some(signal)`.
    pub const fn stop_signal(status: i32) -> Option<u32> {
        if status & 0xFF == 0x7F {
            Some(((status >> 8) & 0xFF) as u32)
        } else {
            None
        }
    }

    /// Whether the status reports a child that continued.
    pub const fn continued(status: i32) -> bool {
        status == 0xFFFF
    }
}

/// A process as [`nr::PROC_INFO`] describes it.
///
/// On the wire, it is [`ProcInfo::SIZE`] bytes: `pid`, `ppid`, `pgid` and `sid` as
/// native-endian `u64`s, then `state` (one byte) and `name` (16 bytes, NUL-padded).
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
    /// `R` running or ready, `S` blocked, `T` stopped, `Z` zombie.
    pub state: u8,
    /// The process name, NUL-padded.
    pub name: [u8; 16],
}

impl ProcInfo {
    /// Size of the record in bytes.
    pub const SIZE: usize = 4 * 8 + 1 + 16;

    /// The record as it is transferred.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        for (i, v) in [self.pid, self.ppid, self.pgid, self.sid]
            .iter()
            .enumerate()
        {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
        }
        b[32] = self.state;
        b[33..].copy_from_slice(&self.name);
        b
    }

    /// Decodes a transferred record.
    pub fn from_bytes(b: &[u8; Self::SIZE]) -> Self {
        let word = |i: usize| {
            let mut w = [0; 8];
            w.copy_from_slice(&b[i * 8..i * 8 + 8]);
            u64::from_ne_bytes(w)
        };
        let mut name = [0; 16];
        name.copy_from_slice(&b[33..]);
        ProcInfo {
            pid: word(0),
            ppid: word(1),
            pgid: word(2),
            sid: word(3),
            state: b[32],
            name,
        }
    }

    /// The name up to its first NUL (`"?"` if it is not UTF-8).
    pub fn name(&self) -> &str {
        let n = self.name.iter().position(|&c| c == 0).unwrap_or(16);
        core::str::from_utf8(&self.name[..n]).unwrap_or("?")
    }
}
