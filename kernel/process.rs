// SPDX-License-Identifier: GPL-2.0

//! Processes.
//!
//! Process management, the cooperative round-robin scheduler and inter-process communication.
//!
//! A process is a private address space plus a kernel context (stack and saved registers, see
//! [`crate::arch::context`]). All user-to-kernel transitions build a [`TrapFrame`] at the top of
//! the process's kernel stack and leave through `trap_return` (`iretq`).
//!
//! IPC comes in two flavours:
//!
//! - Asynchronous notifications ([`ipc_send`]): two words into a small per-process mailbox; the
//!   kernel uses the same path for IRQs.
//! - Synchronous calls ([`ipc_call`]): the client blocks until the server replies. While the call
//!   is pending, the server may copy data from and to the client's buffer (`ipc_read` /
//!   `ipc_write`), as far as the call's [`Access`] allows. So requests of any size need no kernel
//!   buffering, and a server can defer a reply (e.g. a console `read` until a line has been typed)
//!   without blocking itself. Every process has at most one call outstanding, so callers simply
//!   queue in the process table.
//!
//! Processes also carry the POSIX job-control state: a process group and a session (inherited
//! across `fork`, set by `setpgid` / `setsid`), a disposition for every signal, the blocked
//! signals and the pending ones. How a signal travels from [`kill`] to a handler is described at
//! the start of the signals section below.
//!
//! All scheduler state lives in one `Table` behind a [`Lock`]. Functions take it for short,
//! explicit scopes and always release it before switching context or idling.

use crate::arch::entry::TrapFrame;
use crate::arch::multiboot::{self, Module};
use crate::arch::x86::{PageFlag, RFlag};
use crate::arch::{context, cpu};
use crate::signal::{self, Action, Disposition, Frame, SigAction, SigSet};
use crate::sync::{Guard, Lock};
use crate::{elf, fs, gdt, irq, syscall, vmm};
use haisuli_abi::errno::*;
use haisuli_abi::fcntl::{F_DUPFD, OPEN_MAX};
use haisuli_abi::mman::{PROT_EXEC, PROT_WRITE};
use haisuli_abi::signal::{
    NSIG, SA_RESETHAND, SA_RESTART, SIGCONT, SIGKILL, SIGSEGV, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK,
};
use haisuli_abi::{nr, proc_state, svc, ProcInfo, IO_PORTS, KMSG_IRQ, PID_KERNEL};
use haisuli_abi::{Termination, PROC_INFO_LEN, WNOHANG};

pub(crate) const MAX_PROCS: usize = 16;
const MBOX_SIZE: usize = 8;
const USER_STACK_PAGES: u64 = 8;
const NAME_LEN: usize = 16;
/// RFLAGS of a new process: interrupts enabled (and the reserved bit 1).
pub(crate) const USER_RFLAGS: u64 = RFlag::bits(&[RFlag::Interrupt, RFlag::Reserved1]);
/// A file descriptor that is not open.
const NO_FD: u8 = u8::MAX;
/// Descriptors per process.
const FDS: usize = OPEN_MAX as usize;
/// The pid of init, the first boot module.
///
/// The system shuts down when init exits.
const INIT_PID: u64 = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Unused,
    Ready,
    BlockedRecv,
    BlockedWait,
    /// In `ipc_call`, waiting for the server's reply.
    BlockedCall,
    /// Waiting for a well-known service (the console) to register.
    BlockedService,
    /// In `pause`, waiting for a signal.
    BlockedPause,
    /// Stopped by a signal, until SIGCONT (or SIGKILL) arrives.
    Stopped,
    Zombie,
}

#[derive(Clone, Copy)]
struct Msg {
    from: u64,
    a: u64,
    b: u64,
}

/// What the server of a call may do with the client's buffer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Only `ipc_read` (e.g. the buffer of a forwarded `write()`).
    ReadOnly,
    /// Only `ipc_write` (e.g. the buffer of a forwarded `read()`).
    WriteOnly,
    /// Both (an explicit `ipc_call`).
    ReadWrite,
}

impl Access {
    fn allows(self, to_client: bool) -> bool {
        match self {
            Access::ReadOnly => !to_client,
            Access::WriteOnly => to_client,
            Access::ReadWrite => true,
        }
    }
}

/// The outstanding `ipc_call` of a process in `State::BlockedCall`.
#[derive(Clone, Copy)]
struct Call {
    server: u64,
    op: u64,
    buf: u64,
    len: u64,
    access: Access,
    /// Arrival order, so the server receives calls first come, first served.
    seq: u64,
    /// Whether the server has picked the call up with `ipc_recv` yet.
    received: bool,
    /// Set by `ipc_reply` (or to -EPIPE if the server dies, -EINTR if a
    /// signal interrupts the call).
    result: i64,
    /// Whether a caught signal may interrupt the call (a console `read`).
    interruptible: bool,
}

struct Process {
    pid: u64,
    ppid: u64,
    /// Process group, inherited across `fork` and changed by `setpgid`.
    pgid: u64,
    /// Session, inherited across `fork` and changed by `setsid`.
    sid: u64,
    state: State,
    /// The state to return to when a stopped process is continued.
    stopped_prev: State,
    /// The disposition of each signal, by number (inherited across `fork`).
    actions: [SigAction; NSIG as usize],
    /// Signals that are not delivered for now (inherited across `fork`).
    blocked: SigSet,
    /// Signals sent to the process that have not acted on it yet.
    pending: SigSet,
    /// The pid that sent each pending signal (for `siginfo_t.si_pid`).
    senders: [u64; NSIG as usize],
    /// File descriptors: the open file description each refers to ([`fs`]),
    /// or [`NO_FD`].
    fds: [u8; FDS],
    pml4: u64,
    wstatus: i32,
    mmap_next: u64,
    mbox: [Msg; MBOX_SIZE],
    mb_head: usize,
    mb_len: usize,
    name: Name,
    /// Whether the process may use `ioperm`, bind IRQs and register services.
    ///
    /// Boot-module servers may; the flag is not inherited across `fork`.
    privileged: bool,
    io_bitmap: gdt::IoBitmap,
    /// IRQ lines that fired and have not been delivered by `ipc_recv` yet.
    irq_pending: u16,
    call: Call,
}

impl Process {
    const EMPTY: Process = Process {
        pid: 0,
        ppid: 0,
        pgid: 0,
        sid: 0,
        state: State::Unused,
        stopped_prev: State::Ready,
        actions: [SigAction::DEFAULT; NSIG as usize],
        blocked: SigSet::EMPTY,
        pending: SigSet::EMPTY,
        senders: [0; NSIG as usize],
        fds: [NO_FD; FDS],
        pml4: 0,
        wstatus: 0,
        mmap_next: vmm::MMAP_BASE,
        mbox: [Msg {
            from: 0,
            a: 0,
            b: 0,
        }; MBOX_SIZE],
        mb_head: 0,
        mb_len: 0,
        name: Name([0; NAME_LEN]),
        privileged: false,
        io_bitmap: gdt::IO_DENY_ALL,
        irq_pending: 0,
        call: Call {
            server: 0,
            op: 0,
            buf: 0,
            len: 0,
            access: Access::ReadWrite,
            seq: 0,
            received: false,
            result: 0,
            interruptible: false,
        },
    };

    fn alive(&self) -> bool {
        !matches!(self.state, State::Unused | State::Zombie)
    }

    /// Makes a blocked process runnable again.
    ///
    /// A stopped process stays stopped: it only remembers to be runnable once
    /// it is continued, so that a reply or a message cannot un-stop it.
    fn make_ready(&mut self) {
        if self.state == State::Stopped {
            self.stopped_prev = State::Ready;
        } else {
            self.state = State::Ready;
        }
    }

    /// What `ps` shows for this process.
    fn reported_state(&self) -> u64 {
        match self.state {
            State::Unused => 0,
            State::Ready => proc_state::RUNNING,
            State::Stopped => proc_state::STOPPED,
            State::Zombie => proc_state::ZOMBIE,
            _ => proc_state::SLEEPING,
        }
    }
}

/// A process name (NUL-padded).
#[derive(Clone, Copy)]
struct Name([u8; NAME_LEN]);

impl Name {
    fn new(s: &[u8]) -> Name {
        let mut n = [0; NAME_LEN];
        let len = s.len().min(NAME_LEN - 1);
        n[..len].copy_from_slice(&s[..len]);
        Name(n)
    }
}

impl core::fmt::Display for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let n = self.0.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        f.write_str(core::str::from_utf8(&self.0[..n]).unwrap_or("?"))
    }
}

struct Table {
    procs: [Process; MAX_PROCS],
    /// Slot of the running process.
    current: usize,
    call_seq: u64,
    /// Well-known services: pid registered for each service id (0 = none).
    services: [u64; svc::MAX as usize],
}

impl Table {
    fn cur(&self) -> &Process {
        &self.procs[self.current]
    }

    fn cur_mut(&mut self) -> &mut Process {
        &mut self.procs[self.current]
    }

    /// Slot of a live (not unused, not zombie) process. pid = slot + 1.
    fn live_slot(&self, pid: u64) -> Option<usize> {
        if pid == 0 || pid > MAX_PROCS as u64 {
            return None;
        }
        let slot = (pid - 1) as usize;
        self.procs[slot].alive().then_some(slot)
    }

    fn free_slot(&self) -> Option<usize> {
        (0..MAX_PROCS).find(|&i| self.procs[i].state == State::Unused)
    }

    fn wake_receiver(&mut self, slot: usize) {
        if self.procs[slot].state == State::BlockedRecv {
            self.procs[slot].make_ready();
        }
    }
}

static TABLE: Lock<Table> = Lock::new(Table {
    procs: [Process::EMPTY; MAX_PROCS],
    current: 0,
    call_seq: 0,
    services: [0; svc::MAX as usize],
});

fn table() -> Guard<'static, Table> {
    TABLE.lock()
}

pub(crate) fn current_pid() -> u64 {
    table().cur().pid
}
pub(crate) fn current_pml4() -> u64 {
    table().cur().pml4
}
pub(crate) fn current_privileged() -> bool {
    table().cur().privileged
}
pub(crate) fn is_alive(pid: u64) -> bool {
    table().live_slot(pid).is_some()
}

/// Starts every ELF boot module as a privileged process, in module order.
///
/// The first becomes pid 1, init. A module that is not an ELF file is the
/// boot ramdisk, a ustar archive: it is unpacked into the file system
/// ([`fs::init`]).
pub(crate) fn spawn_boot_modules() {
    let ramdisk = multiboot::modules().find(|m| !elf::is_elf(m.data));
    fs::init(ramdisk.map_or(&[][..], |r| r.data));
    let mut started = 0;
    for m in multiboot::modules().filter(|m| elf::is_elf(m.data)) {
        match spawn(&m) {
            Ok(pid) => {
                crate::println!(
                    "[kernel] started pid {} ({}): {} KiB image",
                    pid,
                    Name::new(m.name()),
                    m.data.len().div_ceil(1024),
                );
                started += 1;
            }
            Err(e) => crate::println!(
                "[kernel] cannot start boot module \"{}\": {}",
                Name::new(m.name()),
                e
            ),
        }
    }
    if let Some(r) = ramdisk {
        crate::println!(
            "[kernel] ramdisk \"{}\": {} KiB",
            Name::new(r.name()),
            r.data.len().div_ceil(1024)
        );
    }
    if started == 0 {
        panic!("no runnable boot modules (boot with the modules listed in the README)");
    }
}

fn spawn(m: &Module) -> Result<u64, &'static str> {
    let slot = table().free_slot().ok_or("process table full")?;
    let pml4 = vmm::new_address_space().ok_or("out of memory")?;
    let setup = || -> Result<TrapFrame, &'static str> {
        let entry = elf::load(pml4, m.data)?;
        let stack = USER_STACK_PAGES * 4096;
        let stack_flags =
            PageFlag::bits(&[PageFlag::User, PageFlag::Writable, PageFlag::NoExecute]);
        vmm::map_init(pml4, vmm::USER_STACK_TOP - stack, stack, &[], stack_flags)
            .map_err(|_| "out of memory")?;
        Ok(TrapFrame {
            rip: entry,
            cs: gdt::USER_CS,
            rflags: USER_RFLAGS,
            rsp: vmm::USER_STACK_TOP,
            ss: gdt::USER_DS,
            ..TrapFrame::default()
        })
    };
    let frame = match setup() {
        Ok(f) => f,
        Err(e) => {
            vmm::destroy_user_space(pml4);
            return Err(e);
        }
    };
    context::init(slot, &frame);
    let pid = slot as u64 + 1;
    table().procs[slot] = Process {
        pid,
        // A boot module has no parent and leads its own session.
        pgid: pid,
        sid: pid,
        state: State::Ready,
        pml4,
        privileged: true,
        name: Name::new(m.name()),
        // Standard input, output and error: the terminal.
        fds: {
            let mut fds = [NO_FD; FDS];
            fds[..3].fill(fs::CONSOLE as u8);
            fds
        },
        ..Process::EMPTY
    };
    Ok(pid)
}

/// Make `slot`'s address space, kernel stack and I/O permissions current.
fn activate(slot: usize) {
    let (pml4, io_bitmap) = {
        let t = table();
        (t.procs[slot].pml4, t.procs[slot].io_bitmap)
    };
    let top = context::stack_top(slot);
    vmm::switch(pml4);
    gdt::set_kernel_stack(top);
    gdt::set_io_bitmap(&io_bitmap);
    syscall::set_kernel_stack(top);
}

/// Leave the boot stack for good and run the first process.
pub(crate) fn start() -> ! {
    let first = {
        let mut t = table();
        let first = (0..MAX_PROCS)
            .find(|&i| t.procs[i].state == State::Ready)
            .expect("no process");
        t.current = first;
        first
    };
    activate(first);
    context::switch_to(first);
    unreachable!();
}

fn switch_to(next: usize) {
    {
        let mut t = table();
        if t.current == next {
            return;
        }
        t.current = next;
    }
    activate(next);
    context::switch_to(next);
}

/// Returns whether an interrupt can still make progress possible.
///
/// That is the case if some IRQ line's owner is waiting in `ipc_recv`: its
/// notification will wake it up.
fn irq_can_wake() -> bool {
    let t = table();
    (0..irq::LINES).any(|line| {
        t.live_slot(irq::owner(line))
            .is_some_and(|s| t.procs[s].state == State::BlockedRecv)
    })
}

/// Switches to the next ready process (round robin).
///
/// Returns when the caller is scheduled again. With nothing runnable, idles
/// until an interrupt wakes a driver; if that cannot happen either, stops the
/// machine.
pub(crate) fn schedule() {
    loop {
        let next = {
            let t = table();
            (1..=MAX_PROCS)
                .map(|off| (t.current + off) % MAX_PROCS)
                .find(|&i| t.procs[i].state == State::Ready)
        };
        if let Some(i) = next {
            switch_to(i);
            return;
        }
        if !irq_can_wake() {
            break;
        }
        cpu::wait_for_interrupt(); // The interrupt handler may make someone ready.
    }
    if table().procs.iter().any(Process::alive) {
        crate::println!("[kernel] deadlock: processes blocked, none runnable");
    } else {
        crate::println!("[kernel] all processes have exited - shutting down");
    }
    cpu::qemu_exit(0);
    cpu::halt();
}

pub(crate) fn yield_now() -> i64 {
    schedule();
    0
}

/// What `make_zombie` leaves for its caller to clean up outside the lock.
struct Dead {
    pid: u64,
    name: Name,
    pml4: u64,
    wstatus: i32,
    fds: [u8; FDS],
}

/// Turns `slot` into a zombie: the process stops running, but its wait status
/// stays until its parent reaps it.
///
/// Its children are reparented to init, the calls it was serving fail, and its
/// parent wakes up from `wait`. The caller must pass the result to
/// [`finish_death`] once it has dropped the process table.
fn make_zombie(t: &mut Table, slot: usize, wstatus: i32) -> Dead {
    let p = &mut t.procs[slot];
    let pml4 = core::mem::replace(&mut p.pml4, 0);
    let fds = core::mem::replace(&mut p.fds, [NO_FD; FDS]);
    p.wstatus = wstatus;
    p.state = State::Zombie;
    let (pid, ppid, name) = (p.pid, p.ppid, p.name);
    release_server_state(t, pid);
    for p in t.procs.iter_mut() {
        if p.ppid == pid && p.state != State::Unused {
            p.ppid = INIT_PID; // POSIX: orphans are reparented to init.
        }
        if p.pid == ppid && p.state == State::BlockedWait {
            p.make_ready();
        }
    }
    Dead {
        pid,
        name,
        pml4,
        wstatus,
        fds,
    }
}

/// Frees what a zombie no longer needs and reports the exit on the kernel log.
///
/// Must be called without the process table locked, and only once the dead
/// process's address space is no longer the active one.
fn finish_death(d: Dead) {
    irq::release_all(d.pid);
    for &ofd in d.fds.iter().filter(|&&fd| fd != NO_FD) {
        fs::release(ofd as usize);
    }
    crate::println!(
        "[kernel] pid {} ({}) exited (wait status {:#x})",
        d.pid,
        d.name,
        d.wstatus
    );
    vmm::destroy_user_space(d.pml4);
    if d.pid == INIT_PID {
        crate::println!("[kernel] init exited - shutting down");
        cpu::qemu_exit(0);
        cpu::halt();
    }
}

/// Terminate the current process. `wstatus` uses the POSIX wait-status
/// encoding: (exit_code << 8) for normal exit, or a signal number.
pub(crate) fn exit_current(wstatus: i32) -> ! {
    let dead = {
        let mut t = table();
        let slot = t.current;
        make_zombie(&mut t, slot, wstatus)
    };
    vmm::switch(vmm::kernel_pml4()); // Never free the active page tables.
    finish_death(dead);
    schedule();
    cpu::halt();
}

/// Fails the pending calls and drops the service registrations of `pid`, which is gone.
///
/// Its IRQ lines are released separately.
fn release_server_state(t: &mut Table, pid: u64) {
    for p in t.procs.iter_mut() {
        if p.state == State::BlockedCall && p.call.server == pid {
            p.call.result = -EPIPE;
            p.make_ready();
        }
    }
    for s in t.services.iter_mut() {
        if *s == pid {
            *s = 0;
        }
    }
}

pub(crate) fn getpid() -> i64 {
    current_pid() as i64
}
pub(crate) fn getppid() -> i64 {
    table().cur().ppid as i64
}

/// Creates a copy of the current process (the `fork` system call).
///
/// The child is an eager (non-COW) copy and resumes with `rax` = 0. It does
/// not inherit privileges, I/O permissions or IRQ bindings.
pub(crate) fn fork(frame: &TrapFrame) -> i64 {
    let (slot, parent) = {
        let t = table();
        let p = t.cur();
        // What the child inherits: its parent's process group and session,
        // signal dispositions and mask (not its pending signals), and file
        // descriptors, which share the parent's open file descriptions.
        let child = Process {
            ppid: p.pid,
            pgid: p.pgid,
            sid: p.sid,
            actions: p.actions,
            blocked: p.blocked,
            fds: p.fds,
            mmap_next: p.mmap_next,
            name: p.name,
            ..Process::EMPTY
        };
        (t.free_slot(), (child, p.pml4))
    };
    let Some(slot) = slot else {
        return -EAGAIN;
    };
    let (child, parent_pml4) = parent;
    let Some(pml4) = vmm::clone_user_space(parent_pml4) else {
        return -ENOMEM;
    };
    let mut child_frame = *frame;
    child_frame.rax = 0;
    context::init(slot, &child_frame);
    let pid = slot as u64 + 1;
    for &ofd in child.fds.iter().filter(|&&fd| fd != NO_FD) {
        fs::retain(ofd as usize);
    }
    table().procs[slot] = Process {
        pid,
        state: State::Ready,
        pml4,
        ..child
    };
    pid as i64
}

// ---- File descriptors ---------------------------------------------------------

/// The open file description that the caller's descriptor `fd` refers to.
pub(crate) fn description(fd: u64) -> Option<usize> {
    let t = table();
    match t.cur().fds.get(fd as usize) {
        Some(&ofd) if ofd != NO_FD => Some(ofd as usize),
        _ => None,
    }
}

/// Gives description `ofd` the caller's lowest free descriptor that is at
/// least `min`, handing over the reference the caller holds.
fn install(ofd: usize, min: u64) -> i64 {
    let fd = {
        let mut t = table();
        let fds = &mut t.cur_mut().fds;
        let fd = (min as usize..FDS).find(|&fd| fds[fd] == NO_FD);
        if let Some(fd) = fd {
            fds[fd] = ofd as u8;
        }
        fd
    };
    match fd {
        Some(fd) => fd as i64,
        None => {
            fs::release(ofd);
            -EMFILE
        }
    }
}

/// Opens a file (the `open` system call), given the path the caller passed.
pub(crate) fn open(path: &[u8], flags: u64) -> i64 {
    match fs::open(path, flags) {
        Ok(ofd) => install(ofd, 0),
        Err(e) => e,
    }
}

/// Closes descriptor `fd` (the `close` system call).
pub(crate) fn close(fd: u64) -> i64 {
    let ofd = {
        let mut t = table();
        match t.cur_mut().fds.get_mut(fd as usize) {
            Some(slot) if *slot != NO_FD => core::mem::replace(slot, NO_FD),
            _ => return -EBADF,
        }
    };
    fs::release(ofd as usize);
    0
}

/// Duplicates `fd` onto the lowest free descriptor that is at least `min`
/// (the `dup` system call, and `fcntl(F_DUPFD)`).
pub(crate) fn dup(fd: u64, min: u64) -> i64 {
    let Some(ofd) = description(fd) else {
        return -EBADF;
    };
    if min >= FDS as u64 {
        return -EINVAL;
    }
    fs::retain(ofd);
    install(ofd, min)
}

/// Makes `newfd` refer to what `fd` refers to, closing it first (the `dup2`
/// system call).
pub(crate) fn dup2(fd: u64, newfd: u64) -> i64 {
    let Some(ofd) = description(fd) else {
        return -EBADF;
    };
    if newfd >= FDS as u64 {
        return -EBADF;
    }
    if fd == newfd {
        return newfd as i64;
    }
    fs::retain(ofd);
    let old = core::mem::replace(&mut table().cur_mut().fds[newfd as usize], ofd as u8);
    if old != NO_FD {
        fs::release(old as usize);
    }
    newfd as i64
}

/// The `fcntl` system call: only `F_DUPFD`.
pub(crate) fn fcntl(fd: u64, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => dup(fd, arg),
        _ => -EINVAL,
    }
}

/// Waits for a child to exit and reaps it (the `wait4` system call).
///
/// `want` selects the children to wait for the POSIX way: a pid waits for that
/// child, -1 for any child, 0 for the caller's process group and anything
/// below -1 for process group `-want`. Without a matching child the call fails
/// with `-ECHILD` (so waiting twice for the same child fails, because reaping
/// it made it stop being one); with `WNOHANG` and no exited child it reports 0.
pub(crate) fn wait4(want: i64, status_ptr: u64, options: u64) -> i64 {
    loop {
        let mut t = table();
        let (me, my_pgid, pml4) = (t.cur().pid, t.cur().pgid, t.cur().pml4);
        let mut have_child = false;
        for p in t.procs.iter_mut() {
            if p.state == State::Unused || p.ppid != me {
                continue;
            }
            let wanted = match want {
                -1 => true,
                0 => p.pgid == my_pgid,
                w if w > 0 => p.pid == w as u64,
                w => p.pgid == (-w) as u64,
            };
            if !wanted {
                continue;
            }
            have_child = true;
            if p.state == State::Zombie {
                if status_ptr != 0 && !vmm::copy_to_user(pml4, status_ptr, &p.wstatus.to_ne_bytes())
                {
                    return -EFAULT;
                }
                p.state = State::Unused; // The slot and kernel stack are reusable.
                return p.pid as i64;
            }
        }
        if !have_child {
            return -ECHILD;
        }
        if options & WNOHANG != 0 {
            return 0;
        }
        if interrupting(t.cur()) {
            return -EINTR;
        }
        t.cur_mut().state = State::BlockedWait;
        drop(t);
        schedule();
    }
}

// ---- Signals ----------------------------------------------------------------
//
// A signal is first *posted* (`post`): it becomes pending, unless the target
// ignores it. What happens then depends on its disposition:
//
// - The default action of a signal that is not blocked happens at once (`act_on`):
//   the target is terminated or stopped, whoever sent the signal.
// - A caught signal waits for the target to return to user mode
//   (`deliver_signals`, run at the end of every system call), where the kernel
//   pushes a signal frame and enters the handler. If the target is asleep in an
//   interruptible call (`pause`, `wait4`, `ipc_recv`, a console `read`), it is
//   woken and that call fails with `-EINTR`, or is restarted with `SA_RESTART`.
//
// A stopped process only reacts to SIGKILL and SIGCONT; everything else stays
// pending until it is continued. Since the scheduler is cooperative, a process
// only ever stops running inside a system call, so the end of `syscall_dispatch`
// is the one place where handlers need to be entered.

/// Sleeps until a signal arrives (the `pause` system call).
///
/// Returns `-EINTR` once a handler is about to run; a signal whose default
/// action applies terminates or stops the process instead.
pub(crate) fn pause() -> i64 {
    loop {
        {
            let mut t = table();
            if interrupting(t.cur()) {
                return -EINTR;
            }
            t.cur_mut().state = State::BlockedPause;
        }
        schedule();
    }
}

/// Whether `p` has a caught signal that it does not block: its blocking
/// system call must end with `-EINTR` so that the handler can run.
fn interrupting(p: &Process) -> bool {
    p.pending
        .without(p.blocked)
        .iter()
        .any(|sig| p.actions[sig as usize].disposition(sig) == Disposition::Catch)
}

/// Sets the disposition of a signal (the `rt_sigaction` system call).
///
/// `act` and `oldact` point at Linux `struct sigaction`s (handler, flags,
/// restorer, mask); either may be null. A handler needs `SA_RESTORER`: the
/// restorer is where the handler returns to, and must call `rt_sigreturn`.
pub(crate) fn rt_sigaction(sig: u64, act: u64, oldact: u64, sigsetsize: u64) -> i64 {
    if sigsetsize != 8 || sig == 0 || !signal::valid(sig) {
        return -EINVAL;
    }
    let pml4 = current_pml4();
    let new = if act == 0 {
        None
    } else {
        let mut bytes = [0u8; SigAction::SIZE];
        if !vmm::copy_from_user(pml4, act, &mut bytes) {
            return -EFAULT;
        }
        let new = SigAction::from_bytes(&bytes);
        if !signal::can_change_disposition(sig) || !new.is_valid() {
            return -EINVAL;
        }
        Some(new)
    };
    let old = table().cur().actions[sig as usize];
    if oldact != 0 && !vmm::copy_to_user(pml4, oldact, &old.to_bytes()) {
        return -EFAULT;
    }
    if let Some(new) = new {
        let mut t = table();
        let p = t.cur_mut();
        p.actions[sig as usize] = new;
        if new.disposition(sig) == Disposition::Ignore {
            p.pending.remove(sig); // POSIX: an ignored signal is discarded.
        }
    }
    0
}

/// Changes the caller's blocked signals (the `rt_sigprocmask` system call).
///
/// SIGKILL and SIGSTOP are silently left unblocked. Signals that become
/// unblocked are delivered when the call returns.
pub(crate) fn rt_sigprocmask(how: u64, set: u64, oldset: u64, sigsetsize: u64) -> i64 {
    if sigsetsize != 8 {
        return -EINVAL;
    }
    let pml4 = current_pml4();
    let old = table().cur().blocked;
    let new = if set == 0 {
        None
    } else {
        let mut bytes = [0u8; 8];
        if !vmm::copy_from_user(pml4, set, &mut bytes) {
            return -EFAULT;
        }
        let set = SigSet::from_bits(u64::from_ne_bytes(bytes));
        Some(match how {
            SIG_BLOCK => old.union(set),
            SIG_UNBLOCK => old.without(set),
            SIG_SETMASK => set,
            _ => return -EINVAL,
        })
    };
    if oldset != 0 && !vmm::copy_to_user(pml4, oldset, &old.bits().to_ne_bytes()) {
        return -EFAULT;
    }
    if let Some(new) = new {
        table().cur_mut().blocked = new.blockable();
    }
    0
}

/// Returns from a signal handler (the `rt_sigreturn` system call).
///
/// The handler's `ret` went to its restorer, which made this call with the
/// stack pointer at the frame's `ucontext`. Restores the registers and signal
/// mask saved there, and returns the interrupted `rax`, so that the system
/// call dispatcher hands it back unchanged. A frame that cannot be read, or
/// that would resume at a kernel address, is fatal: SIGSEGV, as on Linux.
pub(crate) fn rt_sigreturn(f: &mut TrapFrame) -> i64 {
    let pml4 = current_pml4();
    let mut saved = signal::SavedContext::empty();
    // The handler's `ret` popped the frame's first word, so the stack pointer
    // is at the `ucontext`.
    let blocked = if vmm::copy_from_user(pml4, f.rsp, saved.as_mut_bytes()) {
        saved.restore(f)
    } else {
        None
    };
    match blocked {
        Some(blocked) => {
            table().cur_mut().blocked = blocked;
            f.rax as i64
        }
        None => exit_current(Termination::Signalled(SIGSEGV).wait_status()),
    }
}

/// Sends `sig` to a process or process group (the `kill` system call).
///
/// `target` follows POSIX: a pid signals that process, 0 the caller's process
/// group, -1 every process that may be signalled (except init and the caller)
/// and anything below -1 the process group `-target`. Signal 0 only reports
/// whether a target exists. A signal the caller sends itself is delivered
/// before `kill` returns, as POSIX requires.
pub(crate) fn kill(target: i64, sig: u64) -> i64 {
    let target = match target {
        -1 => Target::All,
        0 => Target::Group(table().cur().pgid),
        w if w > 0 => Target::Pid(w as u64),
        w => Target::Group((-w) as u64),
    };
    send(target, sig)
}

/// Sends `sig` to every process in group `pgid` (the `kill_pgrp` system call).
///
/// Unlike `kill(-pgid)`, this also reaches group 1 (init's), which is the
/// terminal's foreground group whenever the shell is waiting for input.
pub(crate) fn kill_pgrp(pgid: u64, sig: u64) -> i64 {
    if pgid == 0 {
        return -EINVAL;
    }
    send(Target::Group(pgid), sig)
}

/// The processes a signal is sent to.
enum Target {
    Pid(u64),
    Group(u64),
    /// Every process that may be signalled: all but init and the sender.
    All,
}

/// Sends `sig` to `target`: the common part of `kill` and `kill_pgrp`.
fn send(target: Target, sig: u64) -> i64 {
    if !signal::valid(sig) {
        return -EINVAL;
    }
    let mut targets: u16 = 0;
    let me = {
        let t = table();
        let me = t.cur().pid;
        for (slot, p) in t.procs.iter().enumerate() {
            if !p.alive() {
                continue;
            }
            let hit = match target {
                Target::All => p.pid != me && p.pid != INIT_PID,
                Target::Group(pgid) => p.pgid == pgid,
                Target::Pid(pid) => p.pid == pid,
            };
            if hit {
                targets |= 1 << slot;
            }
        }
        me
    };
    if targets == 0 {
        return -ESRCH;
    }
    if sig == 0 {
        return 0; // Existence check only.
    }
    for slot in (0..MAX_PROCS).filter(|slot| targets & (1 << slot) != 0) {
        let dead = post(&mut table(), slot, sig, me);
        if let Some(d) = dead {
            finish_death(d);
        }
    }
    0
}

/// Posts `sig`, sent by `sender`, to `slot`; returns what to clean up if that
/// terminated the process.
///
/// The running process is never terminated or stopped here (it is still on
/// its own kernel stack): its signals act when it returns to user mode.
fn post(t: &mut Table, slot: usize, sig: u64, sender: u64) -> Option<Dead> {
    let running = t.current == slot;
    let p = &mut t.procs[slot];
    if !p.alive() {
        return None; // It exited while this signal was on its way.
    }
    if sig == SIGKILL && !running {
        return Some(make_zombie(
            t,
            slot,
            Termination::Signalled(sig).wait_status(),
        ));
    }
    let disposition = p.actions[sig as usize].disposition(sig);
    if sig == SIGCONT {
        // Continuing happens whatever the disposition, even if SIGCONT is
        // ignored or blocked.
        p.pending.drop_stops();
        if p.state == State::Stopped {
            p.state = p.stopped_prev;
        }
    } else if signal::default_action(sig) == Action::Stop {
        p.pending.remove(SIGCONT);
    }
    if disposition != Disposition::Ignore || sig == SIGKILL {
        p.pending.add(sig);
        p.senders[sig as usize] = sender;
    }
    if running {
        None
    } else {
        act_on(t, slot)
    }
}

/// Takes the actions that `slot`'s pending signals call for right away.
///
/// For a process that is not running: default actions of unblocked signals
/// happen now, and a caught one interrupts the call it sleeps in.
fn act_on(t: &mut Table, slot: usize) -> Option<Dead> {
    let p = &mut t.procs[slot];
    if p.state == State::Stopped {
        return None; // Only SIGKILL and SIGCONT act on it, in `post`.
    }
    for sig in p.pending.without(p.blocked).iter() {
        match p.actions[sig as usize].disposition(sig) {
            Disposition::Catch => interrupt_sleep(p),
            Disposition::Ignore => p.pending.remove(sig),
            Disposition::Default => match signal::default_action(sig) {
                Action::Term => {
                    p.pending.remove(sig);
                    return Some(make_zombie(
                        t,
                        slot,
                        Termination::Signalled(sig).wait_status(),
                    ));
                }
                Action::Stop => {
                    p.pending.remove(sig);
                    p.stopped_prev = p.state;
                    p.state = State::Stopped;
                    return None;
                }
                Action::Cont | Action::Ignore => p.pending.remove(sig),
            },
        }
    }
    None
}

/// Wakes `p` if it sleeps in a call that a signal interrupts.
///
/// The call notices the pending signal and fails with `-EINTR` (see
/// [`interrupting`]); an interrupted IPC call is withdrawn, so its server can
/// no longer touch the client's buffer.
fn interrupt_sleep(p: &mut Process) {
    match p.state {
        State::BlockedPause | State::BlockedWait | State::BlockedRecv | State::BlockedService => {
            p.state = State::Ready;
        }
        State::BlockedCall if p.call.interruptible => {
            p.call.result = -EINTR;
            p.state = State::Ready;
        }
        _ => {}
    }
}

/// System calls that `SA_RESTART` restarts after a handler ran.
///
/// POSIX: `pause` always fails with `EINTR`.
fn restartable(nr: u64) -> bool {
    matches!(nr, nr::READ | nr::WAIT4 | nr::IPC_RECV)
}

/// What the current process does next with its pending signals.
enum Next {
    /// Nothing is deliverable: return to user mode.
    Return,
    /// Run the handler for `sig`.
    Handle(u64, SigAction, u64, SigSet),
    /// Terminate with the default action of `sig`.
    Die(u64),
    /// Stopped: let others run, then look again.
    Stop,
}

/// Acts on the current process's pending signals on its way back to user mode.
///
/// Called at the end of every system call with the user registers, after
/// `f.rax` got the result of call number `nr`. Default actions happen here
/// for signals the process sent itself or just unblocked; a caught signal gets
/// a signal frame on the user stack and `f` is pointed at its handler.
pub(crate) fn deliver_signals(f: &mut TrapFrame, nr: u64) {
    loop {
        let next = {
            let mut t = table();
            let p = t.cur_mut();
            match p.pending.without(p.blocked).lowest() {
                None => Next::Return,
                Some(sig) => {
                    p.pending.remove(sig);
                    let act = p.actions[sig as usize];
                    match act.disposition(sig) {
                        Disposition::Catch => {
                            let old_blocked = p.blocked;
                            p.blocked = p.blocked.union(act.blocks_while_handling(sig));
                            if act.flags & SA_RESETHAND != 0 {
                                p.actions[sig as usize] = SigAction::DEFAULT;
                            }
                            Next::Handle(sig, act, p.senders[sig as usize], old_blocked)
                        }
                        Disposition::Ignore => continue,
                        Disposition::Default => match signal::default_action(sig) {
                            Action::Term => Next::Die(sig),
                            Action::Stop => {
                                p.stopped_prev = State::Ready;
                                p.state = State::Stopped;
                                Next::Stop
                            }
                            Action::Cont | Action::Ignore => continue,
                        },
                    }
                }
            }
        };
        match next {
            Next::Return => return,
            Next::Die(sig) => exit_current(Termination::Signalled(sig).wait_status()),
            Next::Stop => schedule(),
            Next::Handle(sig, act, sender, old_blocked) => {
                if f.rax as i64 == -EINTR && act.flags & SA_RESTART != 0 && restartable(nr) {
                    // Back up to the `syscall` instruction (2 bytes), so that
                    // returning from the handler makes the call again.
                    f.rip = f.rip.wrapping_sub(2);
                    f.rax = nr;
                }
                enter_handler(f, sig, &act, sender, old_blocked);
                return; // Further signals are delivered when it calls again.
            }
        }
    }
}

/// Pushes the signal frame for `sig` and points `f` at its handler.
///
/// A process whose stack cannot take the frame is killed with SIGSEGV.
fn enter_handler(f: &mut TrapFrame, sig: u64, act: &SigAction, sender: u64, blocked: SigSet) {
    let frame = Frame::new(sig, sender, act, f, blocked);
    // `copy_to_user` only accepts mapped, writable user memory.
    match Frame::address(f.rsp) {
        Some(addr) if vmm::copy_to_user(current_pml4(), addr, frame.as_bytes()) => {
            Frame::enter_handler(f, sig, act, addr);
        }
        _ => exit_current(Termination::Signalled(SIGSEGV).wait_status()),
    }
}

// ---- Process groups and sessions ---------------------------------------------

/// Puts `pid` into process group `pgid` (the `setpgid` system call).
///
/// `pid` 0 means the caller and `pgid` 0 the target's own pid. As POSIX
/// requires, only the caller or one of its children can be moved, a session
/// leader cannot change group, and the group must be in the caller's session.
pub(crate) fn setpgid(pid: u64, pgid: u64) -> i64 {
    let mut t = table();
    let (me, my_sid) = (t.cur().pid, t.cur().sid);
    let pid = if pid == 0 { me } else { pid };
    let Some(slot) = t.live_slot(pid) else {
        return -ESRCH;
    };
    let target = &t.procs[slot];
    if target.pid != me && target.ppid != me {
        return -ESRCH;
    }
    if target.pid == target.sid {
        return -EPERM; // A session leader cannot leave its group.
    }
    if target.sid != my_sid {
        return -EPERM;
    }
    let pgid = if pgid == 0 { pid } else { pgid };
    // The group must be the target's own, or an existing one in the session.
    if pgid != pid
        && !t
            .procs
            .iter()
            .any(|p| p.alive() && p.pgid == pgid && p.sid == my_sid)
    {
        return -EPERM;
    }
    t.procs[slot].pgid = pgid;
    0
}

/// Returns the process group of `pid` (0 = the caller), for `getpgid`.
pub(crate) fn getpgid(pid: u64) -> i64 {
    let t = table();
    let pid = if pid == 0 { t.cur().pid } else { pid };
    match t.live_slot(pid) {
        Some(slot) => t.procs[slot].pgid as i64,
        None => -ESRCH,
    }
}

/// Returns the session of `pid` (0 = the caller), for `getsid`.
pub(crate) fn getsid(pid: u64) -> i64 {
    let t = table();
    let pid = if pid == 0 { t.cur().pid } else { pid };
    match t.live_slot(pid) {
        Some(slot) => t.procs[slot].sid as i64,
        None => -ESRCH,
    }
}

/// Makes the caller the leader of a new session (the `setsid` system call).
pub(crate) fn setsid() -> i64 {
    let mut t = table();
    let me = t.cur().pid;
    // A process group leader cannot start a session: its group would straddle
    // two sessions.
    if t.procs.iter().any(|p| p.alive() && p.pgid == me) {
        return -EPERM;
    }
    let p = t.cur_mut();
    p.sid = me;
    p.pgid = me;
    me as i64
}

/// Lists the processes into the caller's buffer (the `proc_list` system call).
///
/// Each entry is a serialised [`ProcInfo`]; `ps` formats them. The return
/// value is the number of entries written.
pub(crate) fn proc_list(buf: u64, len: u64) -> i64 {
    let pml4 = current_pml4();
    if !vmm::user_range_ok(pml4, buf, len, true) {
        return -EFAULT;
    }
    let capacity = len as usize / PROC_INFO_LEN;
    let mut written = 0;
    for slot in 0..MAX_PROCS {
        if written == capacity {
            break;
        }
        let info = {
            let t = table();
            let p = &t.procs[slot];
            if p.state == State::Unused {
                continue;
            }
            ProcInfo {
                pid: p.pid,
                ppid: p.ppid,
                pgid: p.pgid,
                sid: p.sid,
                state: p.reported_state(),
                name: p.name.0,
            }
        };
        let at = buf + (written * PROC_INFO_LEN) as u64;
        if !vmm::copy_to_user(pml4, at, &info.to_bytes()) {
            return -EFAULT;
        }
        written += 1;
    }
    written as i64
}

/// Anonymous private mapping (addr hint ignored; kernel picks the address).
pub(crate) fn mmap(len: u64, prot: u64) -> i64 {
    if len == 0 || len > (64 << 20) {
        return -EINVAL;
    }
    let pages = len.div_ceil(4096);
    let (pml4, base) = {
        let t = table();
        (t.cur().pml4, t.cur().mmap_next)
    };
    let mut flags = PageFlag::User as u64;
    if prot & PROT_WRITE != 0 {
        flags |= PageFlag::Writable as u64;
    }
    if prot & PROT_EXEC == 0 {
        flags |= PageFlag::NoExecute as u64;
    }
    if vmm::map_init(pml4, base, pages * 4096, &[], flags).is_err() {
        return -ENOMEM;
    }
    table().cur_mut().mmap_next = base + (pages + 1) * 4096; // Leave a guard page.
    base as i64
}

/// Grants or revokes access to I/O ports `[from, from + num)` (the `ioperm` system call).
///
/// Only privileged processes may use it, and only for ports below `IO_PORTS`.
pub(crate) fn ioperm(from: u64, num: u64, turn_on: u64) -> i64 {
    let mut t = table();
    let p = t.cur_mut();
    if !p.privileged {
        return -EPERM;
    }
    match from.checked_add(num) {
        Some(end) if end <= IO_PORTS => {}
        _ => return -EINVAL,
    }
    for port in from..from + num {
        let (byte, bit) = ((port / 8) as usize, port % 8);
        if turn_on != 0 {
            p.io_bitmap[byte] &= !(1 << bit); // 0 means allowed.
        } else {
            p.io_bitmap[byte] |= 1 << bit;
        }
    }
    gdt::set_io_bitmap(&p.io_bitmap); // Takes effect on return to user mode.
    0
}

// ---- IPC: asynchronous per-process mailbox (microkernel message passing) ----

pub(crate) fn ipc_send(dest: u64, a: u64, b: u64) -> i64 {
    let mut t = table();
    let from = t.cur().pid;
    let Some(slot) = t.live_slot(dest) else {
        return -ESRCH;
    };
    let d = &mut t.procs[slot];
    if d.mb_len == MBOX_SIZE {
        return -EAGAIN; // The mailbox is full.
    }
    let tail = (d.mb_head + d.mb_len) % MBOX_SIZE;
    d.mbox[tail] = Msg { from, a, b };
    d.mb_len += 1;
    t.wake_receiver(slot);
    0
}

/// Called from the interrupt handler: line `line` fired for `pid`.
pub(crate) fn irq_notify(pid: u64, line: u64) {
    let mut t = table();
    if let Some(slot) = t.live_slot(pid) {
        t.procs[slot].irq_pending |= 1 << line;
        t.wake_receiver(slot);
    }
}

/// Waits for the next message and writes `[sender_pid, word0, word1]` to `out`.
///
/// Delivers, in order of priority: IRQ notifications `[0, KMSG_IRQ, line]`,
/// mailbox messages, then the oldest pending call `[client, op, len]`.
pub(crate) fn ipc_recv(out: u64) -> i64 {
    let pml4 = current_pml4();
    if !vmm::user_range_ok(pml4, out, 24, true) {
        return -EFAULT;
    }
    loop {
        let mut t = table();
        let Some(msg) = next_message(&mut t) else {
            if interrupting(t.cur()) {
                return -EINTR;
            }
            t.cur_mut().state = State::BlockedRecv;
            drop(t);
            schedule();
            continue;
        };
        drop(t);
        let mut bytes = [0u8; 24];
        bytes[0..8].copy_from_slice(&msg.from.to_ne_bytes());
        bytes[8..16].copy_from_slice(&msg.a.to_ne_bytes());
        bytes[16..24].copy_from_slice(&msg.b.to_ne_bytes());
        vmm::copy_to_user(pml4, out, &bytes);
        return 0;
    }
}

/// Take the current process's next message, if any.
fn next_message(t: &mut Table) -> Option<Msg> {
    let me = t.cur_mut();
    if me.irq_pending != 0 {
        let line = me.irq_pending.trailing_zeros() as u64;
        me.irq_pending &= !(1 << line);
        return Some(Msg {
            from: PID_KERNEL,
            a: KMSG_IRQ,
            b: line,
        });
    }
    if me.mb_len > 0 {
        let m = me.mbox[me.mb_head];
        me.mb_head = (me.mb_head + 1) % MBOX_SIZE;
        me.mb_len -= 1;
        return Some(m);
    }
    let server = me.pid;
    let client = t
        .procs
        .iter_mut()
        .filter(|p| p.state == State::BlockedCall && p.call.server == server && !p.call.received)
        .min_by_key(|p| p.call.seq)?;
    client.call.received = true;
    Some(Msg {
        from: client.pid,
        a: client.call.op,
        b: client.call.len,
    })
}

// ---- IPC: synchronous calls -------------------------------------------------

/// Sends request `op` with buffer `buf[..len]` to `dest` and waits for the reply.
///
/// Returns the reply status. `access` limits what the server may do with the
/// buffer (the `IPC_CALL` system call grants both reading and writing). An
/// `interruptible` call fails with `-EINTR` when a caught signal arrives; it is
/// then withdrawn, so the server can no longer reach the buffer.
pub(crate) fn ipc_call(
    dest: u64,
    op: u64,
    buf: u64,
    len: u64,
    access: Access,
    interruptible: bool,
) -> i64 {
    {
        let mut t = table();
        if interruptible && interrupting(t.cur()) {
            return -EINTR;
        }
        let Some(server) = t.live_slot(dest) else {
            return -ESRCH;
        };
        if server == t.current {
            return -EDEADLK;
        }
        if !vmm::user_range_ok(t.cur().pml4, buf, len, false) {
            return -EFAULT;
        }
        t.call_seq += 1;
        let seq = t.call_seq;
        let p = t.cur_mut();
        p.call = Call {
            server: dest,
            op,
            buf,
            len,
            access,
            seq,
            received: false,
            result: 0,
            interruptible,
        };
        p.state = State::BlockedCall;
        t.wake_receiver(server);
    }
    schedule(); // Until `ipc_reply` (or the server's death) makes us ready.
    table().cur().call.result
}

/// The slot of `client` if it has a call to the current process that the
/// current process has received and not yet replied to.
fn received_call(t: &Table, client: u64) -> Result<usize, i64> {
    let slot = t.live_slot(client).ok_or(-ESRCH)?;
    let c = &t.procs[slot];
    if c.state == State::BlockedCall && c.call.server == t.cur().pid && c.call.received {
        Ok(slot)
    } else {
        Err(-EINVAL)
    }
}

/// Completes a received call from `client` (the `ipc_reply` system call).
pub(crate) fn ipc_reply(client: u64, status: u64) -> i64 {
    let mut t = table();
    match received_call(&t, client) {
        Ok(slot) => {
            let c = &mut t.procs[slot];
            c.call.result = status as i64;
            c.make_ready();
            0
        }
        Err(e) => e,
    }
}

/// Copies between the caller's `local[..len]` and `client`'s call buffer at `offset`.
///
/// This implements the `ipc_read` and `ipc_write` system calls. It copies at
/// most the rest of the client's buffer and returns the number of bytes
/// copied, or `-EACCES` if the call's [`Access`] does not allow this
/// direction.
pub(crate) fn ipc_transfer(client: u64, offset: u64, local: u64, len: u64, to_client: bool) -> i64 {
    let (call, client_pml4, my_pml4) = {
        let t = table();
        match received_call(&t, client) {
            Ok(slot) => (t.procs[slot].call, t.procs[slot].pml4, t.cur().pml4),
            Err(e) => return e,
        }
    };
    if !call.access.allows(to_client) {
        return -EACCES;
    }
    if offset > call.len {
        return -EINVAL;
    }
    let n = len.min(call.len - offset);
    let remote = call.buf + offset;
    let ok = if to_client {
        vmm::copy_user_to_user(client_pml4, remote, my_pml4, local, n)
    } else {
        vmm::copy_user_to_user(my_pml4, local, client_pml4, remote, n)
    };
    if ok {
        n as i64
    } else {
        -EFAULT
    }
}

// ---- Well-known services ----------------------------------------------------

/// Registers the current, privileged process as service `id` (the `svc_register` system call).
pub(crate) fn svc_register(id: u64) -> i64 {
    if id == 0 || id >= svc::MAX {
        return -EINVAL;
    }
    let mut t = table();
    if !t.cur().privileged {
        return -EPERM;
    }
    let me = t.cur().pid;
    let holder = t.services[id as usize];
    if holder != 0 && holder != me && t.live_slot(holder).is_some() {
        return -EBUSY;
    }
    t.services[id as usize] = me;
    for p in t.procs.iter_mut() {
        if p.state == State::BlockedService {
            p.state = State::Ready; // Retry their lookup.
        }
    }
    0
}

/// Returns the pid providing service `id` (the `svc_lookup` system call).
pub(crate) fn svc_lookup(id: u64) -> i64 {
    if id == 0 || id >= svc::MAX {
        return -EINVAL;
    }
    let t = table();
    let pid = t.services[id as usize];
    match t.live_slot(pid) {
        Some(_) => pid as i64,
        None => -ESRCH,
    }
}

/// Forwards `read(0)` / `write(1|2)` to the console server as an `ipc_call`.
///
/// The call lets the server only write / only read the caller's buffer.
/// During boot the console may not have registered yet: wait for it.
///
/// A `read` may wait for a long time, for someone to type a line, so a caught
/// signal interrupts it; a `write` always completes.
pub(crate) fn console_call(op: u64, buf: u64, len: u64, access: Access) -> i64 {
    let interruptible = op == haisuli_abi::console::OP_READ;
    loop {
        let pid = svc_lookup(svc::CONSOLE);
        if pid > 0 {
            // The console server itself must drive the hardware directly
            // (a call to itself fails with EDEADLK).
            return ipc_call(pid as u64, op, buf, len, access, interruptible);
        }
        {
            let mut t = table();
            if interruptible && interrupting(t.cur()) {
                return -EINTR;
            }
            t.cur_mut().state = State::BlockedService;
        }
        schedule();
    }
}
