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
    /// `()`: lets the other ready processes run.
    pub const SCHED_YIELD: u64 = 24;
    /// `()`: returns the caller's process id.
    pub const GETPID: u64 = 39;
    /// `()`: creates a copy of the caller, which returns 0 in the child.
    pub const FORK: u64 = 57;
    /// `(code)`: terminates the caller with exit status `code`.
    pub const EXIT: u64 = 60;
    /// `(pid, status, options, rusage)`: waits for any child to exit (`pid` = -1).
    pub const WAIT4: u64 = 61;
    /// `()`: returns the process id of the caller's parent.
    pub const GETPPID: u64 = 110;
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
}

/// Error numbers (Linux values), returned negated.
pub mod errno {
    /// Operation not permitted (e.g. a privileged call from an ordinary process).
    pub const EPERM: i64 = 1;
    /// No such process (or no such registered service).
    pub const ESRCH: i64 = 3;
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
}
