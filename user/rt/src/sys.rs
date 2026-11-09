// SPDX-License-Identifier: GPL-2.0

//! System calls.
//!
//! Safe wrappers for every HaisuliOS system call; errors are returned as `Err(Errno)`.
//!
//! Every call goes through `invoke`, which takes a `Sys` value whose memory arguments are Rust
//! borrows: a pointer passed to the kernel therefore always comes from a live borrow with its exact
//! length, and from a `&mut` whenever the kernel (or, for IPC, the server) may write through it.

use crate::abi::nr;
use core::arch::asm;

/// An error number from [`crate::abi::errno`] (positive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i64);

/// The result of a system call.
pub type Result<T> = core::result::Result<T, Errno>;

/// Raw system call, for experiments outside the safe wrappers below.
///
/// # Safety
/// The arguments must be valid for syscall `n`: in particular, any memory
/// the call reads or writes must be valid and not aliased in a way that
/// Rust's borrowing rules forbid.
#[inline(always)]
pub unsafe fn syscall(n: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    // SAFETY: The caller guarantees the arguments are valid for call `n`.
    // The kernel preserves every register except rax; rcx and r11 are
    // clobbered by the `syscall` instruction itself.
    unsafe {
        asm!("syscall",
             inlateout("rax") n as i64 => ret,
             in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, in("r8") a4, in("r9") a5,
             lateout("rcx") _, lateout("r11") _,
             options(nostack));
    }
    ret
}

/// The system calls this runtime makes.
enum Sys<'a> {
    /// The kernel lets the console server only write into `buf`.
    Read {
        fd: u64,
        buf: &'a mut [u8],
    },
    /// The kernel lets the console server only read `buf` (its `ipc_write`
    /// fails with `EACCES`), so a shared borrow suffices.
    Write {
        fd: u64,
        buf: &'a [u8],
    },
    Exit(i32),
    GetPid,
    GetPpid,
    Yield,
    Ioperm {
        from: u64,
        num: u64,
        on: bool,
    },
    IpcSend {
        dest: u64,
        a: u64,
        b: u64,
    },
    IpcRecv(&'a mut Message),
    /// The server may read and write `buf` until it replies.
    IpcCall {
        dest: u64,
        op: u64,
        buf: &'a mut [u8],
    },
    IpcReply {
        client: u64,
        status: i64,
    },
    IpcRead {
        client: u64,
        offset: usize,
        dst: &'a mut [u8],
    },
    IpcWrite {
        client: u64,
        offset: usize,
        src: &'a [u8],
    },
    IrqBind(u64),
    IrqAck(u64),
    SvcRegister(u64),
    SvcLookup(u64),
    DebugLog(&'a [u8]),
}

fn invoke(call: Sys<'_>) -> i64 {
    fn ro(b: &[u8]) -> (u64, u64) {
        (b.as_ptr() as u64, b.len() as u64)
    }
    fn rw(b: &mut [u8]) -> (u64, u64) {
        (b.as_mut_ptr() as u64, b.len() as u64)
    }
    let (n, args) = match call {
        Sys::Read { fd, buf } => {
            let (p, l) = rw(buf);
            (nr::READ, [fd, p, l, 0])
        }
        Sys::Write { fd, buf } => {
            let (p, l) = ro(buf);
            (nr::WRITE, [fd, p, l, 0])
        }
        Sys::Exit(code) => (nr::EXIT, [code as u64, 0, 0, 0]),
        Sys::GetPid => (nr::GETPID, [0; 4]),
        Sys::GetPpid => (nr::GETPPID, [0; 4]),
        Sys::Yield => (nr::SCHED_YIELD, [0; 4]),
        Sys::Ioperm { from, num, on } => (nr::IOPERM, [from, num, on as u64, 0]),
        Sys::IpcSend { dest, a, b } => (nr::IPC_SEND, [dest, a, b, 0]),
        Sys::IpcRecv(m) => (nr::IPC_RECV, [m as *mut Message as u64, 0, 0, 0]),
        Sys::IpcCall { dest, op, buf } => {
            let (p, l) = rw(buf);
            (nr::IPC_CALL, [dest, op, p, l])
        }
        Sys::IpcReply { client, status } => (nr::IPC_REPLY, [client, status as u64, 0, 0]),
        Sys::IpcRead {
            client,
            offset,
            dst,
        } => {
            let (p, l) = rw(dst);
            (nr::IPC_READ, [client, offset as u64, p, l])
        }
        Sys::IpcWrite {
            client,
            offset,
            src,
        } => {
            let (p, l) = ro(src);
            (nr::IPC_WRITE, [client, offset as u64, p, l])
        }
        Sys::IrqBind(line) => (nr::IRQ_BIND, [line, 0, 0, 0]),
        Sys::IrqAck(line) => (nr::IRQ_ACK, [line, 0, 0, 0]),
        Sys::SvcRegister(id) => (nr::SVC_REGISTER, [id, 0, 0, 0]),
        Sys::SvcLookup(id) => (nr::SVC_LOOKUP, [id, 0, 0, 0]),
        Sys::DebugLog(msg) => {
            let (p, l) = ro(msg);
            (nr::DEBUG_LOG, [p, l, 0, 0])
        }
    };
    // SAFETY: Every pointer in `args` was taken from a borrow in `call`,
    // which lives until this function returns, with its exact length; it is
    // a `&mut` borrow wherever the kernel or a server can write memory (see
    // `Sys`; for `Write` the kernel itself forbids the server to write). The
    // remaining calls take no pointers.
    unsafe { syscall(n, args[0], args[1], args[2], args[3], 0, 0) }
}

fn check(r: i64) -> Result<u64> {
    if r < 0 {
        Err(Errno(-r))
    } else {
        Ok(r as u64)
    }
}

// ---- POSIX subset ----

/// Reads from `fd` into `buf` and returns the number of bytes read (0 at end of file).
pub fn read(fd: u64, buf: &mut [u8]) -> Result<usize> {
    check(invoke(Sys::Read { fd, buf })).map(|n| n as usize)
}

/// Writes `buf` to `fd` and returns the number of bytes written.
pub fn write(fd: u64, buf: &[u8]) -> Result<usize> {
    check(invoke(Sys::Write { fd, buf })).map(|n| n as usize)
}

/// Terminates the calling process with exit status `code`.
pub fn exit(code: i32) -> ! {
    invoke(Sys::Exit(code));
    unreachable!("exit returned")
}

/// Returns the calling process's id.
pub fn getpid() -> u64 {
    invoke(Sys::GetPid) as u64
}

/// Returns the process id of the caller's parent (0 for boot modules).
pub fn getppid() -> u64 {
    invoke(Sys::GetPpid) as u64
}

/// Lets the other ready processes run.
pub fn sched_yield() {
    invoke(Sys::Yield);
}

/// Grants (or revokes) access to I/O ports `[from, from + num)`.
pub fn ioperm(from: u64, num: u64, turn_on: bool) -> Result<()> {
    check(invoke(Sys::Ioperm {
        from,
        num,
        on: turn_on,
    }))
    .map(drop)
}

// ---- IPC ----

/// A received message: `[sender, a, b]`.
///
/// For a call, `a` is the operation and `b` the length of the client's buffer.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Message {
    /// The sending process, or [`PID_KERNEL`](crate::abi::PID_KERNEL) for kernel notifications.
    pub sender: u64,
    /// The first word (the operation, for a call).
    pub a: u64,
    /// The second word (the buffer length, for a call).
    pub b: u64,
}

/// Sends the asynchronous notification `[a, b]` to `dest`.
pub fn ipc_send(dest: u64, a: u64, b: u64) -> Result<()> {
    check(invoke(Sys::IpcSend { dest, a, b })).map(drop)
}

/// Waits for the next message: a notification or a call.
pub fn ipc_recv() -> Result<Message> {
    let mut m = Message::default();
    check(invoke(Sys::IpcRecv(&mut m))).map(|_| m)
}

/// Sends a synchronous request to `dest` and returns the reply status.
///
/// The server may read and write `buf` until it replies.
pub fn ipc_call(dest: u64, op: u64, buf: &mut [u8]) -> Result<u64> {
    check(invoke(Sys::IpcCall { dest, op, buf }))
}

/// Completes `client`'s call, whose `ipc_call` then returns `status`.
///
/// Negative values are seen as errors by the client.
pub fn ipc_reply(client: u64, status: i64) -> Result<()> {
    check(invoke(Sys::IpcReply { client, status })).map(drop)
}

/// Copies from `client`'s call buffer, starting at `offset`.
pub fn ipc_read(client: u64, offset: usize, dst: &mut [u8]) -> Result<usize> {
    check(invoke(Sys::IpcRead {
        client,
        offset,
        dst,
    }))
    .map(|n| n as usize)
}

/// Copies into `client`'s call buffer, starting at `offset`.
pub fn ipc_write(client: u64, offset: usize, src: &[u8]) -> Result<usize> {
    check(invoke(Sys::IpcWrite {
        client,
        offset,
        src,
    }))
    .map(|n| n as usize)
}

// ---- Drivers and services ----

/// Routes hardware IRQ `line` to this (privileged) process.
pub fn irq_bind(line: u64) -> Result<()> {
    check(invoke(Sys::IrqBind(line))).map(drop)
}

/// Re-enables IRQ `line` after its device has been serviced.
pub fn irq_ack(line: u64) -> Result<()> {
    check(invoke(Sys::IrqAck(line))).map(drop)
}

/// Registers this (privileged) process as well-known service `id`.
pub fn svc_register(id: u64) -> Result<()> {
    check(invoke(Sys::SvcRegister(id))).map(drop)
}

/// Returns the pid of the process providing service `id`.
pub fn svc_lookup(id: u64) -> Result<u64> {
    check(invoke(Sys::SvcLookup(id)))
}

/// Writes to the kernel debug log, which never blocks.
pub fn debug_log(msg: &[u8]) {
    invoke(Sys::DebugLog(msg));
}
