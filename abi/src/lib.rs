// SPDX-License-Identifier: GPL-2.0

//! The HaisuliOS ABI.
//!
//! System call numbers, error numbers, IPC protocols and device descriptions shared by the kernel
//! and all user programs.
//!
//! Sharing one crate means the two sides cannot drift apart. The system call convention is the
//! architecture's Linux one: on x86_64 the `syscall` instruction, with the number in `rax`, the
//! arguments in `rdi rsi rdx r10 r8 r9` and the result in `rax`; on arm64 `svc #0`, with the
//! number in `x8`, the arguments in `x0` to `x5` and the result in `x0`. A negative result is an
//! error number. File, memory and thread calls use the architecture's Linux numbers (see [`nr`]);
//! HaisuliOS-native calls start at 0x1000, the same on every architecture. POSIX processes are not
//! system calls: they are the process server's protocol, [`proc`].
//!
//! A *task* is an address space with one or more *threads*. Both have ids from the same range: a
//! task's id is the id of the thread it started with (its *leader*), and stays its id after that
//! thread has ended, until the task ends. IPC addresses tasks; calls are made, received and
//! answered by threads.

#![no_std]

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("HaisuliOS runs on x86_64 and arm64 (aarch64) only");

/// The value of a constant on the architecture being built for, from a list of
/// `"architecture" => value` pairs (one for every architecture that has the constant).
macro_rules! per_arch {
    ($($arch:literal => $value:expr),+ $(,)?) => {{
        $(
            #[cfg(target_arch = $arch)]
            const VALUE: u64 = $value;
        )+
        VALUE
    }};
}

/// System call numbers.
///
/// The kernel implements mechanisms only. POSIX process management (`fork`, `exit`, `wait4`,
/// `getpid`, process groups, signals...) is not a system call: the process server implements it,
/// and programs reach it through the [`proc`] protocol.
///
/// The calls that Linux has too take its numbers, which differ between architectures: x86_64 has
/// its own table, arm64 the generic one. Some x86_64 calls do not exist on arm64 at all: there,
/// [`OPENAT`](nr::OPENAT) replaces `open` and [`DUP3`](nr::DUP3) replaces `dup2`, which
/// HaisuliOS has on x86_64 only (so that a libc for x86_64 finds them). The HaisuliOS-native
/// calls, from 0x1000 on, are the same everywhere.
pub mod nr {
    // Files and memory, with Linux numbers.

    /// `(fd, buf, len)`: reads from a file, or from the terminal (the console server).
    pub const READ: u64 = per_arch!("x86_64" => 0, "aarch64" => 63);
    /// `(fd, buf, len)`: writes to a file, or to the terminal (the console server).
    pub const WRITE: u64 = per_arch!("x86_64" => 1, "aarch64" => 64);
    /// `(path, flags, mode)`: opens a file and returns the lowest free descriptor; x86_64 only
    /// (`openat(AT_FDCWD, path, flags, mode)` everywhere).
    ///
    /// `path` is NUL-terminated; `flags` take the [`O_*`](super::fcntl) values.
    #[cfg(target_arch = "x86_64")]
    pub const OPEN: u64 = 2;
    /// `(dirfd, path, flags, mode)`: opens a file and returns the lowest free descriptor.
    ///
    /// A relative `path` starts at directory `dirfd`, or at the root for
    /// [`AT_FDCWD`](super::fcntl::AT_FDCWD) (there is no current directory); an absolute one
    /// ignores `dirfd`. `path` is NUL-terminated; `flags` take the [`O_*`](super::fcntl) values.
    pub const OPENAT: u64 = per_arch!("x86_64" => 257, "aarch64" => 56);
    /// `(fd)`: closes a file descriptor.
    pub const CLOSE: u64 = per_arch!("x86_64" => 3, "aarch64" => 57);
    /// `(addr, len, prot, flags, fd, offset)`: maps anonymous memory; the kernel picks the address.
    pub const MMAP: u64 = per_arch!("x86_64" => 9, "aarch64" => 222);
    /// `()`: lets the other ready threads run.
    pub const SCHED_YIELD: u64 = per_arch!("x86_64" => 24, "aarch64" => 124);
    /// `(fd)`: duplicates `fd` onto the lowest free descriptor.
    pub const DUP: u64 = per_arch!("x86_64" => 32, "aarch64" => 23);
    /// `(fd, newfd)`: makes `newfd` a copy of `fd`, closing it first if needed; x86_64 only
    /// ([`DUP3`] everywhere).
    #[cfg(target_arch = "x86_64")]
    pub const DUP2: u64 = 33;
    /// `(fd, newfd, flags)`: as `dup2`, but `fd` and `newfd` must differ (`-EINVAL`), and
    /// `flags` must be 0 (there is no `O_CLOEXEC` yet).
    pub const DUP3: u64 = per_arch!("x86_64" => 292, "aarch64" => 24);
    /// `()`: returns from an upcall (a signal handler), restoring the interrupted context.
    ///
    /// Only an upcall's restorer may call it, with the stack as the kernel left it (see
    /// [`upcall`](super::upcall) and [`TASK_UPCALL`]).
    pub const RT_SIGRETURN: u64 = per_arch!("x86_64" => 15, "aarch64" => 139);
    /// `(fd, cmd, arg)`: only `F_DUPFD` (duplicate onto the lowest free descriptor that is at
    /// least `arg`) and `F_GETFD` (0 for an open descriptor: there are no descriptor flags yet).
    pub const FCNTL: u64 = per_arch!("x86_64" => 72, "aarch64" => 25);
    /// `(fd, buf, len)`: reads directory entries (Linux `struct linux_dirent64`).
    ///
    /// Returns the number of bytes filled, 0 at the end of the directory.
    pub const GETDENTS64: u64 = per_arch!("x86_64" => 217, "aarch64" => 61);

    // Threads and CPUs, with Linux numbers.

    /// `(flags, stack, ...)`: starts a thread in the caller's task.
    ///
    /// `flags` must be [`clone::THREAD_FLAGS`](super::clone::THREAD_FLAGS) (new processes come
    /// from the process server's `fork`); the other arguments but `stack` are ignored (their
    /// order differs between the architectures, as on Linux). The new thread shares the task's
    /// address space, file descriptors and everything else; it returns 0 from this call on
    /// `stack`, with every other register as the caller had it. The caller gets the new thread's
    /// id. Fails with `-EAGAIN` when no thread slot is free.
    pub const CLONE: u64 = per_arch!("x86_64" => 56, "aarch64" => 220);
    /// `(status)`: ends the calling thread (Linux `exit`, not `exit_group`).
    ///
    /// When the last thread of a task ends, the task ends as with [`TASK_EXIT`]. Programs end
    /// their process through the process server instead (its `OP_EXIT`), which ends every thread.
    pub const EXIT: u64 = per_arch!("x86_64" => 60, "aarch64" => 93);
    /// `()`: the calling thread's id.
    pub const GETTID: u64 = per_arch!("x86_64" => 186, "aarch64" => 178);
    /// `(pid, len, mask)`: writes the set of CPUs the caller may run on (every online CPU, one
    /// bit each) to `mask[..len]`.
    ///
    /// `pid` must be 0 (the caller: the kernel knows no pids). Returns the bytes written (8).
    pub const SCHED_GETAFFINITY: u64 = per_arch!("x86_64" => 204, "aarch64" => 123);
    /// `(cpu, node, cache)`: stores the number of the CPU the caller runs on (from 0) in `*cpu`,
    /// and 0 in `*node`; either pointer may be null, `cache` is ignored.
    pub const GETCPU: u64 = per_arch!("x86_64" => 309, "aarch64" => 168);

    // HaisuliOS-native microkernel calls.

    /// `(dest, word0, word1)`: sends an asynchronous, non-blocking notification to task `dest`.
    pub const IPC_SEND: u64 = 0x1000;
    /// `(*mut [u64; 4])`: waits for a message and fills in `[sender, word0, word1, task]`.
    ///
    /// For a call, `sender` is the calling thread (the one to reply to) and `task` its task; for
    /// a notification both are the sending task (0 for the kernel).
    pub const IPC_RECV: u64 = 0x1001;
    /// `(dest, op, buf, len)`: sends a synchronous request to task `dest`.
    ///
    /// Blocks until the server replies and returns the server's reply status.
    /// The server may read and write `buf[..len]` with [`IPC_READ`] /
    /// [`IPC_WRITE`] while the call is pending.
    pub const IPC_CALL: u64 = 0x1002;
    /// `(client, status)`: completes a received call; `client` is the calling thread.
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
    /// `()`: ends the calling task, every thread of it.
    ///
    /// The task manager is told with [`KMSG_TASK_EXIT`](super::KMSG_TASK_EXIT). Programs end
    /// through the process server instead (its `OP_EXIT`); this is for when there is none.
    pub const TASK_EXIT: u64 = 0x1010;
    /// `(thread)`: creates a copy of the task of `thread`, which must be blocked in a call to
    /// the caller.
    ///
    /// The copy gets a copy of the address space and the file descriptors, but no privileges,
    /// and one thread: a copy of `thread`, whose pending call returns 0. Returns the new task's
    /// id. Task manager only.
    pub const TASK_FORK: u64 = 0x1011;
    /// `(tid)`: ends task `tid` at once, every thread of it, wherever they run. Task manager
    /// only.
    pub const TASK_KILL: u64 = 0x1012;
    /// `(tid)`: stops scheduling the threads of task `tid`, wherever they are. Task manager only.
    pub const TASK_STOP: u64 = 0x1013;
    /// `(tid)`: schedules a stopped task again. Task manager only.
    pub const TASK_CONT: u64 = 0x1014;
    /// `(tid, *const [u64; upcall::WORDS])`: makes a thread of task `tid` enter a handler.
    ///
    /// See [`upcall`](super::upcall). Fails with `-EBUSY` while an earlier upcall for the task
    /// has not been entered yet. Task manager only.
    pub const TASK_UPCALL: u64 = 0x1015;
    /// `(tid, buf, len)`: fills `buf` with a [`TaskInfo`](super::TaskInfo) about task `tid`.
    pub const TASK_INFO: u64 = 0x1016;
    /// `(thread, block, len, flags)`: makes the task of `thread` run a program from the file
    /// system.
    ///
    /// `block[..len]` is an [argument block](super::exec) naming the program and its arguments.
    /// `thread` must be blocked in a call to the caller, which it leaves for the new program's
    /// entry point; the task's other threads end. The task keeps its id and its file
    /// descriptors; everything else starts afresh (see [`exec`](super::exec)). Task manager
    /// only.
    pub const TASK_EXEC: u64 = 0x1018;
    /// `(block, len, flags)`: starts a program from the file system as a new task.
    ///
    /// As [`TASK_EXEC`], but in a new task, whose standard input, output and error are the
    /// terminal. Returns the new task's id. Task manager only.
    pub const TASK_SPAWN: u64 = 0x1019;
    /// `()`: powers the machine off. Privileged tasks only.
    pub const POWER_OFF: u64 = 0x1017;
    /// `(buf, len)`: sends `buf[..len]` as it is out of the serial line that the kernel's debug
    /// log uses (the [`device::CONSOLE`](super::device::CONSOLE) device). Privileged tasks only.
    ///
    /// The kernel is the line's one transmitter: what one call sends never mixes with the
    /// kernel's log lines, nor with the output of other CPUs. The console server sends the
    /// terminal's output this way (it still drives the rest of the UART itself).
    pub const SERIAL_WRITE: u64 = 0x101A;
    /// `(device, buf, len)`: fills `buf` with a [`DeviceInfo`](super::device::DeviceInfo) about
    /// device `device` (one of the ids in [`device`](super::device)).
    ///
    /// Fails with `-ENODEV` if the machine has no such device.
    pub const DEVICE_INFO: u64 = 0x101B;
    /// `(device)`: gives the (privileged) caller the registers of device `device`, for a driver.
    ///
    /// For registers in the I/O port space (the PC's), the caller may use those ports from then
    /// on, and gets the first one back; for memory-mapped registers, they are mapped into the
    /// caller's address space (uncached), and it gets their address. The grant lasts until the
    /// program ends (or `execve`s). Fails with `-EPERM` for an unprivileged caller, and with
    /// `-ENODEV` if the machine has no such device.
    pub const DEVICE_GRANT: u64 = 0x101C;
}

/// The flags of [`nr::CLONE`] (Linux values).
pub mod clone {
    /// Share the address space.
    pub const CLONE_VM: u64 = 0x100;
    /// Share the file system information (root, current directory).
    pub const CLONE_FS: u64 = 0x200;
    /// Share the file descriptors.
    pub const CLONE_FILES: u64 = 0x400;
    /// Share the signal dispositions.
    pub const CLONE_SIGHAND: u64 = 0x800;
    /// Be a thread of the same process.
    pub const CLONE_THREAD: u64 = 0x1_0000;
    /// A thread of the caller's task: what HaisuliOS's `clone` does, and all it does.
    pub const THREAD_FLAGS: u64 = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
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
    /// Argument list too long (an [argument block](super::exec) over `ARG_MAX`).
    pub const E2BIG: i64 = 7;
    /// Exec format error: not a program that this system can run.
    pub const ENOEXEC: i64 = 8;
    /// Resource busy (e.g. an IRQ line or service that is already taken).
    pub const EBUSY: i64 = 16;
    /// No such device.
    pub const ENODEV: i64 = 19;
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
            E2BIG => "Argument list too long",
            ENOEXEC => "Exec format error",
            EBADF => "Bad file descriptor",
            ECHILD => "No child processes",
            EAGAIN => "Resource temporarily unavailable",
            ENOMEM => "Cannot allocate memory",
            EACCES => "Permission denied",
            EFAULT => "Bad address",
            EBUSY => "Device or resource busy",
            ENODEV => "No such device",
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

/// File access: `OPENAT` flags, `FCNTL` commands and directory entries (Linux values).
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
    /// Fail with `ENOTDIR` unless the path is a directory (arm64's value differs from x86_64's).
    pub const O_DIRECTORY: u64 = per_arch!("x86_64" => 0o200000, "aarch64" => 0o40000);

    /// `OPENAT`'s `dirfd` for paths relative to the current directory: HaisuliOS has none, so
    /// they start at the root.
    pub const AT_FDCWD: u64 = -100i64 as u64;

    /// `FCNTL`: duplicate onto the lowest free descriptor that is at least `arg`.
    pub const F_DUPFD: u64 = 0;
    /// `FCNTL`: the descriptor's flags (always 0: there is no `FD_CLOEXEC` yet), or `EBADF`.
    pub const F_GETFD: u64 = 1;

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

/// The top of every task's stack, above which its arguments lie when a program starts.
pub const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;

/// Programs: the arguments of [`nr::TASK_EXEC`], [`nr::TASK_SPAWN`] and the process server's
/// [`proc::OP_EXEC`].
///
/// An *argument block* is `[argc: u64, envc: u64]` followed by NUL-terminated strings: the path
/// of the program, then `argc` arguments, then `envc` environment strings.
/// [`build`](exec::build) makes one.
///
/// A program starts with the System V initial stack, as on Linux: the stack pointer points
/// at `argc`, followed by the `argv` pointers and a null pointer, the `envp` pointers and a null
/// pointer, and an empty auxiliary vector (`AT_NULL`). The strings lie above them, below
/// [`USER_STACK_TOP`].
pub mod exec {
    use super::errno::{E2BIG, EINVAL};

    /// Most bytes in an argument block.
    pub const ARG_MAX: usize = 4096;
    /// Most arguments and environment strings together.
    pub const STRINGS_MAX: usize = 256;
    /// Bytes before the strings: `argc` and `envc`.
    pub const HEADER: usize = 16;
    /// Flag: the program runs privileged (only a privileged caller may ask for it).
    pub const PRIVILEGED: u64 = 1;

    /// Writes the argument block for running `path` with `argv` and `envp` into `out`.
    ///
    /// Returns its length, `-E2BIG` if it does not fit or `-EINVAL` if a string contains a NUL.
    pub fn build(path: &str, argv: &[&str], envp: &[&str], out: &mut [u8]) -> Result<usize, i64> {
        if argv.len() + envp.len() > STRINGS_MAX || out.len() < HEADER {
            return Err(-E2BIG);
        }
        out[..8].copy_from_slice(&(argv.len() as u64).to_ne_bytes());
        out[8..HEADER].copy_from_slice(&(envp.len() as u64).to_ne_bytes());
        let mut len = HEADER;
        for s in core::iter::once(&path).chain(argv).chain(envp) {
            if s.as_bytes().contains(&0) {
                return Err(-EINVAL);
            }
            let end = len + s.len() + 1;
            if end > out.len() {
                return Err(-E2BIG);
            }
            out[len..end - 1].copy_from_slice(s.as_bytes());
            out[end - 1] = 0;
            len = end;
        }
        Ok(len)
    }
}

/// Upcalls: the kernel mechanism under POSIX signal handlers.
///
/// [`nr::TASK_UPCALL`] takes [`WORDS`](upcall::WORDS) words:
/// `[handler, restorer, arg, sender, mask, flags]`. When the task next returns to user mode, the
/// kernel pushes the architecture's Linux signal frame (`struct rt_sigframe`) below its stack
/// pointer and enters `handler(arg, &siginfo, &ucontext)`, returning to `restorer`.
/// `siginfo.si_signo` is `arg` and `si_pid` is `sender`. The kernel does not interpret `mask`: it
/// stores it as the frame's `uc_sigmask`, for the restorer to hand back to whoever manages signal
/// masks (it finds it at [`UC_SIGMASK`](upcall::UC_SIGMASK)). The restorer ends with
/// [`nr::RT_SIGRETURN`], which restores the registers saved in the frame.
///
/// Posting an upcall also ends an interruptible wait of the thread that takes it (`ipc_recv`,
/// `svc_wait` or a terminal `read`) with `-EINTR`. The thread enters the handler when it next
/// returns to user mode: at the end of a system call, or of an interrupt.
pub mod upcall {
    /// Words in an upcall request.
    pub const WORDS: usize = 6;
    /// Flag: if the system call the task returns from fails with `-EINTR`, make it again after
    /// the handler (POSIX `SA_RESTART`).
    pub const RESTART: u64 = 1;
    /// Flag: `tid` names a thread of the task, the one to enter the handler. Otherwise the kernel
    /// picks one: a thread in an interruptible wait, else one that is running or ready, else any.
    pub const THREAD: u64 = 2;
    /// Where the frame's `uc_sigmask` (the `mask` of the request) is, counted from the stack
    /// pointer that the restorer starts with.
    ///
    /// That is the frame's layout, which is the architecture's. On x86_64, the handler's return
    /// took the restorer's address (`pretcode`) off the frame, and the stack pointer points at
    /// the `ucontext`, whose `uc_sigmask` is 296 bytes in. On arm64, the handler returns through
    /// the link register, and the stack pointer still points at the frame: `siginfo` (128 bytes),
    /// then the `ucontext`, whose `uc_sigmask` is 40 bytes in.
    pub const UC_SIGMASK: usize = per_arch!("x86_64" => 296, "aarch64" => 128 + 40) as usize;
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
    /// One of [`task_state`]: [`STOPPED`](task_state::STOPPED) if the task is stopped, else
    /// [`READY`](task_state::READY) if a thread of it runs or may run, else
    /// [`BLOCKED`](task_state::BLOCKED).
    pub state: u64,
    /// Whether an upcall waits to be entered: no thread of the task has returned to user mode
    /// since the upcall was requested.
    pub upcall: bool,
    /// How many threads the task has.
    pub threads: u64,
    /// The program name, NUL-padded.
    pub name: [u8; PROC_NAME_LEN],
}

/// Serialised size of a [`TaskInfo`].
pub const TASK_INFO_LEN: usize = 3 * 8 + PROC_NAME_LEN;

impl TaskInfo {
    /// Serialises the entry.
    pub fn to_bytes(&self) -> [u8; TASK_INFO_LEN] {
        let mut b = [0u8; TASK_INFO_LEN];
        b[..8].copy_from_slice(&self.state.to_ne_bytes());
        b[8..16].copy_from_slice(&(self.upcall as u64).to_ne_bytes());
        b[16..24].copy_from_slice(&self.threads.to_ne_bytes());
        b[24..].copy_from_slice(&self.name);
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
        name.copy_from_slice(&b[24..]);
        TaskInfo {
            state: word(0),
            upcall: word(1) != 0,
            threads: word(2),
            name,
        }
    }
}

/// Signal numbers and dispositions (the Linux values, which are the same on x86_64 and arm64).
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
    /// How many threads the process has (0 for a zombie).
    pub threads: u64,
    /// The program name, NUL-padded.
    pub name: [u8; PROC_NAME_LEN],
}

/// Length of a process name in a [`ProcInfo`].
pub const PROC_NAME_LEN: usize = 16;
/// Serialised size of a [`ProcInfo`].
pub const PROC_INFO_LEN: usize = 6 * 8 + PROC_NAME_LEN;

impl ProcInfo {
    /// Serialises the entry.
    pub fn to_bytes(&self) -> [u8; PROC_INFO_LEN] {
        let mut b = [0u8; PROC_INFO_LEN];
        let words = [
            self.pid,
            self.ppid,
            self.pgid,
            self.sid,
            self.state,
            self.threads,
        ];
        for (i, v) in words.iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
        }
        b[48..].copy_from_slice(&self.name);
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
        name.copy_from_slice(&b[48..]);
        ProcInfo {
            pid: word(0),
            ppid: word(1),
            pgid: word(2),
            sid: word(3),
            state: word(4),
            threads: word(5),
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

/// Declares an enum of flags (bits of one 8-bit register) with a `bits` function that combines
/// several of them into the register value.
macro_rules! flags {
    ($(#[$meta:meta])* enum $name:ident: $ty:ty {
        $($(#[$variant_meta:meta])* $variant:ident = $value:expr,)*
    }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq)]
        #[repr($ty)]
        pub enum $name {
            $($(#[$variant_meta])* $variant = $value,)*
        }

        impl $name {
            /// The register value with exactly `flags` set.
            pub const fn bits(flags: &[$name]) -> $ty {
                let mut bits = 0;
                let mut i = 0;
                while i < flags.len() {
                    bits |= flags[i] as $ty;
                    i += 1;
                }
                bits
            }
        }
    };
}

/// The 16550 UART: registers and their bits.
///
/// Shared by the kernel's debug log and the user-space console driver, which
/// program the same device (Linux likewise exports `serial_reg.h` to user
/// space). Register values are written as `bits(&[...])` of their flags.
/// Where a 16550 is (the PC's COM1 is at I/O ports `0x3F8` to `0x3FF`) is the
/// machine's business: see [`device`].
pub mod uart16550 {
    /// Registers of one UART, one byte apart.
    pub const REGISTERS: u64 = 8;
    /// The divisor for 38400 baud (the UART's clock is 115200 Hz).
    pub const DIVISOR_38400: u16 = 3;

    /// The UART's registers, by offset from the first.
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

    flags! {
        /// [`Register::InterruptEnable`] bits.
        enum InterruptEnable: u8 {
            /// A byte has been received.
            ReceivedData = 0x01,
        }
    }

    flags! {
        /// [`Register::FifoControl`] bits.
        enum FifoControl: u8 {
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
        enum LineControl: u8 {
            /// 8 data bits (with no parity and one stop bit: "8N1").
            Word8 = 0x03,
            /// DLAB: registers 0 and 1 hold the baud-rate divisor.
            DivisorLatch = 0x80,
        }
    }

    flags! {
        /// [`Register::ModemControl`] bits.
        enum ModemControl: u8 {
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
        enum LineStatus: u8 {
            /// A received byte is waiting in [`Register::Data`].
            DataReady = 0x01,
            /// The transmit holding register can take another byte.
            TransmitterEmpty = 0x20,
        }
    }
}

/// The ARM PrimeCell UART (PL011): registers and their bits.
///
/// The console device of arm64 machines such as QEMU's `virt`. Its registers are memory-mapped,
/// 32 bits wide, at the offsets of [`Register`](pl011::Register).
pub mod pl011 {
    /// Bytes of its register block.
    pub const REGISTERS: u64 = 0x1000;

    /// The UART's registers, by byte offset.
    #[derive(Clone, Copy, PartialEq, Eq)]
    #[repr(u64)]
    pub enum Register {
        /// Data: the received byte (read), the byte to send (write).
        Data = 0x00,
        /// Flags (see [`Flag`]).
        Flag = 0x18,
        /// Line control: the word format and the FIFOs (see [`LineControl`]).
        LineControl = 0x2C,
        /// Control: enables (see [`Control`]).
        Control = 0x30,
        /// Interrupt mask: which events raise the interrupt (see [`Interrupt`]).
        InterruptMask = 0x38,
        /// Interrupt clear: write the [`Interrupt`] bits to clear.
        InterruptClear = 0x44,
    }

    flags! {
        /// [`Register::Flag`] bits.
        enum Flag: u32 {
            /// The receive FIFO is empty.
            ReceiveEmpty = 1 << 4,
            /// The transmit FIFO is full.
            TransmitFull = 1 << 5,
        }
    }

    flags! {
        /// [`Register::LineControl`] bits.
        enum LineControl: u32 {
            /// Use the FIFOs.
            Fifos = 1 << 4,
            /// 8 data bits (with no parity and one stop bit: "8N1").
            Word8 = 0b11 << 5,
        }
    }

    flags! {
        /// [`Register::Control`] bits.
        enum Control: u32 {
            /// The UART is enabled.
            Enable = 1 << 0,
            /// The transmitter is enabled.
            Transmit = 1 << 8,
            /// The receiver is enabled.
            Receive = 1 << 9,
        }
    }

    flags! {
        /// Interrupt bits ([`Register::InterruptMask`], [`Register::InterruptClear`]).
        enum Interrupt: u32 {
            /// A byte was received (the receive FIFO reached its trigger level).
            Receive = 1 << 4,
            /// Received bytes have waited in the FIFO for a while.
            ReceiveTimeout = 1 << 6,
        }
    }
}

/// Devices: what user-space drivers get from [`nr::DEVICE_INFO`] and [`nr::DEVICE_GRANT`].
///
/// The machine (the kernel's platform) describes its devices; a driver asks for one by a
/// well-known id, learns what it is, where its registers are and which interrupt line it raises,
/// and then gets its registers granted. So the console server runs the same on a PC (a 16550 in
/// the I/O port space, IRQ 4) as on an arm64 machine (a PL011, memory-mapped).
pub mod device {
    use super::PROC_NAME_LEN;

    /// The serial line of the console: the one the kernel's log goes out on, whose input the
    /// console server handles.
    pub const CONSOLE: u64 = 1;

    /// [`DeviceInfo::kind`]: a 16550 UART (see [`uart16550`](super::uart16550)).
    pub const KIND_UART_16550: u64 = 1;
    /// [`DeviceInfo::kind`]: a PL011 UART (see [`pl011`](super::pl011)).
    pub const KIND_UART_PL011: u64 = 2;

    /// [`DeviceInfo::space`]: the registers are I/O ports (the PC's), one per register.
    pub const SPACE_PORTS: u64 = 1;
    /// [`DeviceInfo::space`]: the registers are memory-mapped.
    pub const SPACE_MMIO: u64 = 2;

    /// [`DeviceInfo::irq`] of a device that raises no interrupt.
    pub const NO_IRQ: u64 = u64::MAX;

    /// What [`nr::DEVICE_INFO`](super::nr::DEVICE_INFO) reports about a device.
    ///
    /// It is serialised as [`DEVICE_INFO_LEN`] bytes, so that neither side needs unsafe code to
    /// move it across the system call boundary.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct DeviceInfo {
        /// What the device is: one of the `KIND_*` values.
        pub kind: u64,
        /// Where its registers are: [`SPACE_PORTS`] or [`SPACE_MMIO`].
        pub space: u64,
        /// The first register: an I/O port, or a physical address.
        pub base: u64,
        /// The size of the register block: ports, or bytes.
        pub len: u64,
        /// The interrupt line it raises (for `IRQ_BIND`), or [`NO_IRQ`].
        pub irq: u64,
        /// The machine's name for it (such as `COM1`), NUL-padded.
        pub name: [u8; PROC_NAME_LEN],
    }

    /// Serialised size of a [`DeviceInfo`].
    pub const DEVICE_INFO_LEN: usize = 5 * 8 + PROC_NAME_LEN;

    impl DeviceInfo {
        /// Serialises the description.
        pub fn to_bytes(&self) -> [u8; DEVICE_INFO_LEN] {
            let mut b = [0u8; DEVICE_INFO_LEN];
            let words = [self.kind, self.space, self.base, self.len, self.irq];
            for (i, v) in words.iter().enumerate() {
                b[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
            }
            b[40..].copy_from_slice(&self.name);
            b
        }

        /// Parses a description written by [`to_bytes`](Self::to_bytes).
        pub fn from_bytes(b: &[u8; DEVICE_INFO_LEN]) -> DeviceInfo {
            let word = |i: usize| {
                let mut w = [0u8; 8];
                w.copy_from_slice(&b[i * 8..i * 8 + 8]);
                u64::from_ne_bytes(w)
            };
            let mut name = [0u8; PROC_NAME_LEN];
            name.copy_from_slice(&b[40..]);
            DeviceInfo {
                kind: word(0),
                space: word(1),
                base: word(2),
                len: word(3),
                irq: word(4),
                name,
            }
        }

        /// The device's name as text.
        pub fn name(&self) -> &str {
            let n = self
                .name
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(PROC_NAME_LEN);
            core::str::from_utf8(&self.name[..n]).unwrap_or("?")
        }
    }
}

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
/// The call buffer of a request is an array of native-endian `u64` words, whose meaning is listed
/// with each operation (except for [`OP_LIST`](proc::OP_LIST) and [`OP_EXEC`](proc::OP_EXEC));
/// words marked *out* are written by the server. The reply is the
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
    /// An [argument block](super::exec): `execve`.
    ///
    /// On success there is no reply: the caller runs the new program, with the same pid, process
    /// group, session, file descriptors, signal mask and ignored signals; caught signals go back
    /// to their default action. The program never runs privileged.
    pub const OP_EXEC: u64 = 17;
}
