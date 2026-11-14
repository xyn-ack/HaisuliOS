// SPDX-License-Identifier: GPL-2.0

//! Tasks.
//!
//! Tasks, the cooperative round-robin scheduler and inter-process communication.
//!
//! A task is a private address space plus a kernel context (stack and saved registers, see
//! [`crate::arch::context`]). All user-to-kernel transitions build a [`TrapFrame`] at the top of
//! the task's kernel stack and leave through `trap_return` (`iretq`).
//!
//! IPC comes in two flavours:
//!
//! - Asynchronous notifications ([`ipc_send`]): two words into a small per-task mailbox; the
//!   kernel uses the same path for IRQs.
//! - Synchronous calls ([`ipc_call`]): the client blocks until the server replies. While the call
//!   is pending, the server may copy data from and to the client's buffer (`ipc_read` /
//!   `ipc_write`), as far as the call's [`Access`] allows. So requests of any size need no kernel
//!   buffering, and a server can defer a reply (e.g. a console `read` until a line has been typed)
//!   without blocking itself. Every task has at most one call outstanding, so callers simply
//!   queue in the task table.
//!
//! The kernel knows nothing about POSIX processes. Process ids, parents and children, exit
//! statuses, process groups, sessions and signals are the business of the process server, which
//! the kernel knows only as the *task manager*: the task registered as service [`svc::PROC`]. It
//! alone may use the task control calls, the mechanisms that processes and signals are built on:
//! copying a task ([`task_fork`]), ending ([`task_kill`]), stopping and continuing one, and making
//! one enter a handler ([`task_upcall`]). A task that ends by itself (with [`end_current`]) is
//! reported to it with a kernel notification.
//!
//! All scheduler state lives in one `Table` behind a [`Lock`]. Functions take it for short,
//! explicit scopes and always release it before switching context or idling.

use crate::arch::entry::TrapFrame;
use crate::arch::multiboot::{self, Module};
use crate::arch::x86::{PageFlag, RFlag};
use crate::arch::{context, cpu};
use crate::sync::{Guard, Lock};
use crate::upcall::{Frame, SavedContext, Upcall};
use crate::{elf, fs, gdt, irq, syscall, vmm};
use haisuli_abi::errno::*;
use haisuli_abi::fcntl::{F_DUPFD, OPEN_MAX};
use haisuli_abi::mman::{PROT_EXEC, PROT_WRITE};
use haisuli_abi::{svc, task_state, upcall, TaskInfo, IO_PORTS, TASK_INFO_LEN};
use haisuli_abi::{KMSG_IRQ, KMSG_TASK_EXIT, KMSG_TASK_FAULT, PID_KERNEL};

pub(crate) const MAX_TASKS: usize = 16;
const MBOX_SIZE: usize = 8;
const USER_STACK_PAGES: u64 = 8;
const NAME_LEN: usize = 16;
/// RFLAGS of a new task: interrupts enabled (and the reserved bit 1).
pub(crate) const USER_RFLAGS: u64 = RFlag::bits(&[RFlag::Interrupt, RFlag::Reserved1]);
/// A file descriptor that is not open.
const NO_FD: u8 = u8::MAX;
/// Descriptors per task.
const FDS: usize = OPEN_MAX as usize;
/// Bytes of the `syscall` instruction, to make an interrupted call again.
const SYSCALL_LEN: u64 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Unused,
    Ready,
    BlockedRecv,
    /// In `ipc_call`, waiting for the server's reply.
    BlockedCall,
    /// Waiting for a well-known service to register.
    BlockedService,
    /// Stopped by the task manager, until it continues the task.
    Stopped,
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

/// The outstanding `ipc_call` of a task in `State::BlockedCall`.
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
    /// Set by `ipc_reply` (or to -EPIPE if the server dies, -EINTR if an
    /// upcall interrupts the call).
    result: i64,
    /// Whether an upcall may interrupt the call (a terminal `read`).
    interruptible: bool,
}

struct Task {
    tid: u64,
    state: State,
    /// The state to return to when a stopped task is continued.
    stopped_prev: State,
    /// An upcall to enter on the way back to user mode.
    upcall: Option<Upcall>,
    /// File descriptors: the open file description each refers to ([`fs`]),
    /// or [`NO_FD`].
    fds: [u8; FDS],
    pml4: u64,
    mmap_next: u64,
    mbox: [Msg; MBOX_SIZE],
    mb_head: usize,
    mb_len: usize,
    name: Name,
    /// Whether the task may use `ioperm`, bind IRQs and register services.
    ///
    /// Boot modules may; a copy made with `task_fork` may not.
    privileged: bool,
    io_bitmap: gdt::IoBitmap,
    /// IRQ lines that fired and have not been delivered by `ipc_recv` yet.
    irq_pending: u16,
    call: Call,
}

impl Task {
    const EMPTY: Task = Task {
        tid: 0,
        state: State::Unused,
        stopped_prev: State::Ready,
        upcall: None,
        fds: [NO_FD; FDS],
        pml4: 0,
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
        self.state != State::Unused
    }

    /// Makes a blocked task runnable again.
    ///
    /// A stopped task stays stopped: it only remembers to be runnable once
    /// it is continued, so that a reply or a message cannot un-stop it.
    fn make_ready(&mut self) {
        if self.state == State::Stopped {
            self.stopped_prev = State::Ready;
        } else {
            self.state = State::Ready;
        }
    }

    /// Whether the task is in state `blocked`, or stopped there.
    fn blocked_in(&self, blocked: State) -> bool {
        self.state == blocked || (self.state == State::Stopped && self.stopped_prev == blocked)
    }

    /// Wakes the task if it is blocked in state `blocked` (or stopped there).
    fn wake_from(&mut self, blocked: State) {
        if self.blocked_in(blocked) {
            self.make_ready();
        }
    }

    /// What `TASK_INFO` reports.
    fn reported_state(&self) -> u64 {
        match self.state {
            State::Ready => task_state::READY,
            State::Stopped => task_state::STOPPED,
            _ => task_state::BLOCKED,
        }
    }
}

/// A task name (NUL-padded).
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
    tasks: [Task; MAX_TASKS],
    /// Slot of the running task.
    current: usize,
    call_seq: u64,
    /// Well-known services: task registered for each service id (0 = none).
    services: [u64; svc::MAX as usize],
    /// Slots of tasks that ended themselves, not yet reported to the task manager.
    ///
    /// Such a slot is not reused before the report, so the id in it stays unambiguous.
    ended: u16,
    /// Slots of tasks that faulted, not yet reported to the task manager.
    faulted: u16,
}

impl Table {
    fn cur(&self) -> &Task {
        &self.tasks[self.current]
    }

    fn cur_mut(&mut self) -> &mut Task {
        &mut self.tasks[self.current]
    }

    /// Slot of a live task. tid = slot + 1.
    fn live_slot(&self, tid: u64) -> Option<usize> {
        if tid == 0 || tid > MAX_TASKS as u64 {
            return None;
        }
        let slot = (tid - 1) as usize;
        self.tasks[slot].alive().then_some(slot)
    }

    fn free_slot(&self) -> Option<usize> {
        let unreported = self.ended | self.faulted;
        (0..MAX_TASKS).find(|&i| self.tasks[i].state == State::Unused && unreported & (1 << i) == 0)
    }

    fn wake_receiver(&mut self, slot: usize) {
        self.tasks[slot].wake_from(State::BlockedRecv);
    }

    /// Slot of the task manager (the task registered as [`svc::PROC`]).
    fn manager_slot(&self) -> Option<usize> {
        self.live_slot(self.services[svc::PROC as usize])
    }

    /// The slot of task `tid`, if the running task is the task manager and may control it (any
    /// live task but itself).
    fn controlled_slot(&self, tid: u64) -> Result<usize, i64> {
        if self.manager_slot() != Some(self.current) {
            return Err(-EPERM);
        }
        match self.live_slot(tid) {
            Some(slot) if slot == self.current => Err(-EINVAL),
            Some(slot) => Ok(slot),
            None => Err(-ESRCH),
        }
    }
}

static TABLE: Lock<Table> = Lock::new(Table {
    tasks: [Task::EMPTY; MAX_TASKS],
    current: 0,
    call_seq: 0,
    services: [0; svc::MAX as usize],
    ended: 0,
    faulted: 0,
});

fn table() -> Guard<'static, Table> {
    TABLE.lock()
}

pub(crate) fn current_tid() -> u64 {
    table().cur().tid
}
pub(crate) fn current_pml4() -> u64 {
    table().cur().pml4
}
pub(crate) fn current_privileged() -> bool {
    table().cur().privileged
}
pub(crate) fn is_alive(tid: u64) -> bool {
    table().live_slot(tid).is_some()
}

/// Starts every ELF boot module as a privileged task, in module order.
///
/// The first becomes task 1 (init). A module that is not an ELF file is the
/// boot ramdisk, a ustar archive: it is unpacked into the file system
/// ([`fs::init`]).
///
/// The log calls the task ids pids: the process server adopts the boot tasks
/// as processes under the ids the kernel gave them.
pub(crate) fn spawn_boot_modules() {
    let ramdisk = multiboot::modules().find(|m| !elf::is_elf(m.data));
    fs::init(ramdisk.map_or(&[][..], |r| r.data));
    let mut started = 0;
    for m in multiboot::modules().filter(|m| elf::is_elf(m.data)) {
        match spawn(&m) {
            Ok(tid) => {
                crate::println!(
                    "[kernel] started pid {} ({}): {} KiB image",
                    tid,
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
    let slot = table().free_slot().ok_or("task table full")?;
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
    let tid = slot as u64 + 1;
    table().tasks[slot] = Task {
        tid,
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
        ..Task::EMPTY
    };
    Ok(tid)
}

/// Make `slot`'s address space, kernel stack and I/O permissions current.
fn activate(slot: usize) {
    let (pml4, io_bitmap) = {
        let t = table();
        (t.tasks[slot].pml4, t.tasks[slot].io_bitmap)
    };
    let top = context::stack_top(slot);
    vmm::switch(pml4);
    gdt::set_kernel_stack(top);
    gdt::set_io_bitmap(&io_bitmap);
    syscall::set_kernel_stack(top);
}

/// Leave the boot stack for good and run the first task.
pub(crate) fn start() -> ! {
    let first = {
        let mut t = table();
        let first = (0..MAX_TASKS)
            .find(|&i| t.tasks[i].state == State::Ready)
            .expect("no task");
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
            .is_some_and(|s| t.tasks[s].state == State::BlockedRecv)
    })
}

/// Switches to the next ready task (round robin).
///
/// Returns when the caller is scheduled again. With nothing runnable, idles
/// until an interrupt wakes a driver; if that cannot happen either, stops the
/// machine.
pub(crate) fn schedule() {
    loop {
        let next = {
            let t = table();
            (1..=MAX_TASKS)
                .map(|off| (t.current + off) % MAX_TASKS)
                .find(|&i| t.tasks[i].state == State::Ready)
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
    if table().tasks.iter().any(Task::alive) {
        crate::println!("[kernel] deadlock: tasks blocked, none runnable");
    } else {
        crate::println!("[kernel] all tasks have ended - shutting down");
    }
    cpu::qemu_exit(0);
    cpu::halt();
}

pub(crate) fn yield_now() -> i64 {
    schedule();
    0
}

/// What [`end_task`] leaves for its caller to clean up outside the lock.
struct Dead {
    tid: u64,
    pml4: u64,
    fds: [u8; FDS],
}

/// Ends the task in `slot`: it never runs again, and its slot is free.
///
/// The calls it was serving fail and its services are dropped. The caller must
/// pass the result to [`finish`] once it has dropped the task table.
fn end_task(t: &mut Table, slot: usize) -> Dead {
    let p = &mut t.tasks[slot];
    let dead = Dead {
        tid: p.tid,
        pml4: p.pml4,
        fds: p.fds,
    };
    *p = Task::EMPTY;
    release_server_state(t, dead.tid);
    dead
}

/// Frees what an ended task held.
///
/// Must be called without the task table locked, and only once the task's
/// address space is no longer the active one.
fn finish(d: Dead) {
    irq::release_all(d.tid);
    for &ofd in d.fds.iter().filter(|&&fd| fd != NO_FD) {
        fs::release(ofd as usize);
    }
    vmm::destroy_user_space(d.pml4);
}

/// Why a task ended by itself.
#[derive(Clone, Copy)]
pub(crate) enum Why {
    /// It called `TASK_EXIT`.
    Exit,
    /// It caused a CPU exception, or its upcall frame could not be used.
    Fault,
}

/// Ends the running task and reports it to the task manager.
pub(crate) fn end_current(why: Why) -> ! {
    let dead = {
        let mut t = table();
        let slot = t.current;
        let dead = end_task(&mut t, slot);
        // Ending the task manager itself dropped its registration: then there
        // is nobody to tell.
        if let Some(manager) = t.manager_slot() {
            match why {
                Why::Exit => t.ended |= 1 << slot,
                Why::Fault => t.faulted |= 1 << slot,
            }
            t.wake_receiver(manager);
        }
        dead
    };
    vmm::switch(vmm::kernel_pml4()); // Never free the active page tables.
    finish(dead);
    schedule();
    cpu::halt();
}

/// Fails the pending calls and drops the service registrations of `tid`, which is gone.
///
/// Its IRQ lines are released separately.
fn release_server_state(t: &mut Table, tid: u64) {
    for p in t.tasks.iter_mut() {
        if p.blocked_in(State::BlockedCall) && p.call.server == tid {
            p.call.result = -EPIPE;
            p.make_ready();
        }
    }
    for s in t.services.iter_mut() {
        if *s == tid {
            *s = 0;
        }
    }
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

// ---- Task control (the task manager's mechanisms) ------------------------------

/// Creates a copy of task `tid` (the `task_fork` system call).
///
/// `tid` must be blocked in a call to the caller, the task manager, that it
/// has received. The copy gets an eager (non-COW) copy of the address space,
/// the registers, with the pending call returning 0, and the file descriptors,
/// which share their open file descriptions. It gets no privileges, I/O
/// permissions, IRQs, messages or upcalls. Returns the new task's id.
pub(crate) fn task_fork(tid: u64) -> i64 {
    let (parent_slot, child, parent_pml4, slot) = {
        let t = table();
        let parent_slot = match t.controlled_slot(tid) {
            Ok(slot) => slot,
            Err(e) => return e,
        };
        let p = &t.tasks[parent_slot];
        if !(p.state == State::BlockedCall && p.call.server == t.cur().tid && p.call.received) {
            return -EINVAL;
        }
        let child = Task {
            fds: p.fds,
            mmap_next: p.mmap_next,
            name: p.name,
            ..Task::EMPTY
        };
        (parent_slot, child, p.pml4, t.free_slot())
    };
    let Some(slot) = slot else {
        return -EAGAIN;
    };
    let Some(pml4) = vmm::clone_user_space(parent_pml4) else {
        return -ENOMEM;
    };
    let mut frame = context::user_frame(parent_slot);
    frame.rax = 0; // The result of the copy's call.
    context::init(slot, &frame);
    for &ofd in child.fds.iter().filter(|&&fd| fd != NO_FD) {
        fs::retain(ofd as usize);
    }
    let tid = slot as u64 + 1;
    table().tasks[slot] = Task {
        tid,
        state: State::Ready,
        pml4,
        ..child
    };
    tid as i64
}

/// Ends task `tid` at once (the `task_kill` system call).
///
/// Whatever it was blocked in is abandoned: a call it made is withdrawn, and
/// calls it was serving fail with `-EPIPE`.
pub(crate) fn task_kill(tid: u64) -> i64 {
    let dead = {
        let mut t = table();
        match t.controlled_slot(tid) {
            Ok(slot) => end_task(&mut t, slot),
            Err(e) => return e,
        }
    };
    finish(dead); // The caller's address space is the active one, not the dead task's.
    0
}

/// Stops scheduling task `tid` (the `task_stop` system call).
///
/// The task keeps its place in whatever it was blocked in: a reply or message
/// that arrives meanwhile only makes it runnable once it is continued.
pub(crate) fn task_stop(tid: u64) -> i64 {
    let mut t = table();
    match t.controlled_slot(tid) {
        Ok(slot) => {
            let p = &mut t.tasks[slot];
            if p.state != State::Stopped {
                p.stopped_prev = p.state;
                p.state = State::Stopped;
            }
            0
        }
        Err(e) => e,
    }
}

/// Lets a stopped task `tid` run again (the `task_cont` system call).
pub(crate) fn task_cont(tid: u64) -> i64 {
    let mut t = table();
    match t.controlled_slot(tid) {
        Ok(slot) => {
            let p = &mut t.tasks[slot];
            if p.state == State::Stopped {
                p.state = p.stopped_prev;
            }
            0
        }
        Err(e) => e,
    }
}

/// Makes task `tid` enter a handler (the `task_upcall` system call).
///
/// `request` points at the caller's [`upcall::WORDS`] words (see
/// [`haisuli_abi::upcall`]). The handler is entered when the task next
/// returns to user mode; an interruptible wait ends with `-EINTR` now.
pub(crate) fn task_upcall(tid: u64, request: u64) -> i64 {
    let mut bytes = [0u8; upcall::WORDS * 8];
    if !vmm::copy_from_user(current_pml4(), request, &mut bytes) {
        return -EFAULT;
    }
    let mut words = [0u64; upcall::WORDS];
    for (w, b) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *w = u64::from_ne_bytes(*b);
    }
    let Some(up) = Upcall::from_words(&words) else {
        return -EINVAL;
    };
    let mut t = table();
    let slot = match t.controlled_slot(tid) {
        Ok(slot) => slot,
        Err(e) => return e,
    };
    let p = &mut t.tasks[slot];
    if p.upcall.is_some() {
        return -EBUSY;
    }
    p.upcall = Some(up);
    interrupt_sleep(p);
    0
}

/// Reports on task `tid` (the `task_info` system call): a [`TaskInfo`] in `buf`.
pub(crate) fn task_info(tid: u64, buf: u64, len: u64) -> i64 {
    if len < TASK_INFO_LEN as u64 {
        return -EINVAL;
    }
    let (info, pml4) = {
        let t = table();
        let Some(slot) = t.live_slot(tid) else {
            return -ESRCH;
        };
        let p = &t.tasks[slot];
        let info = TaskInfo {
            state: p.reported_state(),
            upcall: p.upcall.is_some(),
            name: p.name.0,
        };
        (info, t.cur().pml4)
    };
    if !vmm::copy_to_user(pml4, buf, &info.to_bytes()) {
        return -EFAULT;
    }
    0
}

/// Powers the machine off (the `power_off` system call); privileged tasks only.
pub(crate) fn power_off() -> i64 {
    if !current_privileged() {
        return -EPERM;
    }
    crate::println!("[kernel] powering off");
    cpu::qemu_exit(0);
    cpu::halt()
}

// ---- Upcalls ------------------------------------------------------------------
//
// An upcall waits for its task to return to user mode: a task only ever stops
// running inside a system call (the scheduler is cooperative), so the end of
// `syscall_dispatch` is the one place where handlers need to be entered
// ([`return_to_user`]). If the task is asleep in an interruptible wait
// (`ipc_recv`, `svc_wait`, a terminal `read`), the wait ends with `-EINTR`, or
// is made again after the handler if the upcall asks for that.

/// Whether `p` has an upcall waiting: a blocking call must end with `-EINTR`
/// so that the handler can run.
fn interrupting(p: &Task) -> bool {
    p.upcall.is_some()
}

/// Wakes `p` if it sleeps in a wait that an upcall interrupts (also while stopped).
///
/// The wait notices the upcall and fails with `-EINTR` (see [`interrupting`]);
/// an interrupted IPC call is withdrawn, so its server can no longer touch the
/// client's buffer.
fn interrupt_sleep(p: &mut Task) {
    if p.blocked_in(State::BlockedRecv) || p.blocked_in(State::BlockedService) {
        p.make_ready();
    } else if p.blocked_in(State::BlockedCall) && p.call.interruptible {
        p.call.result = -EINTR;
        p.make_ready();
    }
}

/// Enters the running task's upcall, if it has one, on its way back to user mode.
///
/// Called at the end of every system call with the user registers, after
/// `f.rax` got the result of call number `nr`. A task whose stack cannot take
/// the frame has faulted.
pub(crate) fn return_to_user(f: &mut TrapFrame, nr: u64) {
    let Some(up) = table().cur_mut().upcall.take() else {
        return;
    };
    if f.rax as i64 == -EINTR && up.restart {
        // Back up to the `syscall` instruction, so that returning from the
        // handler makes the call again.
        f.rip = f.rip.wrapping_sub(SYSCALL_LEN);
        f.rax = nr;
    }
    let frame = Frame::new(&up, f);
    // `copy_to_user` only accepts mapped, writable user memory.
    match Frame::address(f.rsp) {
        Some(addr) if vmm::copy_to_user(current_pml4(), addr, frame.as_bytes()) => {
            Frame::enter_handler(f, &up, addr);
        }
        _ => end_current(Why::Fault),
    }
}

/// Returns from an upcall handler (the `rt_sigreturn` system call).
///
/// The handler's `ret` went to its restorer, which made this call with the
/// stack pointer at the frame's `ucontext`. Restores the registers saved
/// there, and returns the interrupted `rax`, so that the system call
/// dispatcher hands it back unchanged. A frame that cannot be read, or that
/// would resume at a kernel address, is a fault.
pub(crate) fn rt_sigreturn(f: &mut TrapFrame) -> i64 {
    let mut saved = SavedContext::empty();
    if vmm::copy_from_user(current_pml4(), f.rsp, saved.as_mut_bytes()) && saved.restore(f) {
        f.rax as i64
    } else {
        end_current(Why::Fault)
    }
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
/// Only privileged tasks may use it, and only for ports below `IO_PORTS`.
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

// ---- IPC: asynchronous per-task mailbox (microkernel message passing) ----

pub(crate) fn ipc_send(dest: u64, a: u64, b: u64) -> i64 {
    let mut t = table();
    let from = t.cur().tid;
    let Some(slot) = t.live_slot(dest) else {
        return -ESRCH;
    };
    let d = &mut t.tasks[slot];
    if d.mb_len == MBOX_SIZE {
        return -EAGAIN; // The mailbox is full.
    }
    let tail = (d.mb_head + d.mb_len) % MBOX_SIZE;
    d.mbox[tail] = Msg { from, a, b };
    d.mb_len += 1;
    t.wake_receiver(slot);
    0
}

/// Called from the interrupt handler: line `line` fired for task `tid`.
pub(crate) fn irq_notify(tid: u64, line: u64) {
    let mut t = table();
    if let Some(slot) = t.live_slot(tid) {
        t.tasks[slot].irq_pending |= 1 << line;
        t.wake_receiver(slot);
    }
}

/// Waits for the next message and writes `[sender, word0, word1]` to `out`.
///
/// Delivers, in order of priority: IRQ notifications `[0, KMSG_IRQ, line]`,
/// for the task manager the reports of ended tasks (`[0, KMSG_TASK_EXIT, tid]`
/// and `[0, KMSG_TASK_FAULT, tid]`), mailbox messages, then the oldest pending
/// call `[client, op, len]`.
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

/// Take the current task's next message, if any.
fn next_message(t: &mut Table) -> Option<Msg> {
    let is_manager = t.manager_slot() == Some(t.current);
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
    if is_manager {
        for (kind, slots) in [
            (KMSG_TASK_EXIT, &mut t.ended),
            (KMSG_TASK_FAULT, &mut t.faulted),
        ] {
            if *slots != 0 {
                let slot = slots.trailing_zeros() as u64;
                *slots &= !(1 << slot);
                return Some(Msg {
                    from: PID_KERNEL,
                    a: kind,
                    b: slot + 1,
                });
            }
        }
    }
    let me = t.cur_mut();
    if me.mb_len > 0 {
        let m = me.mbox[me.mb_head];
        me.mb_head = (me.mb_head + 1) % MBOX_SIZE;
        me.mb_len -= 1;
        return Some(m);
    }
    let server = me.tid;
    let client = t
        .tasks
        .iter_mut()
        .filter(|p| p.blocked_in(State::BlockedCall) && p.call.server == server && !p.call.received)
        .min_by_key(|p| p.call.seq)?;
    client.call.received = true;
    Some(Msg {
        from: client.tid,
        a: client.call.op,
        b: client.call.len,
    })
}

// ---- IPC: synchronous calls -------------------------------------------------

/// Sends request `op` with buffer `buf[..len]` to `dest` and waits for the reply.
///
/// Returns the reply status. `access` limits what the server may do with the
/// buffer (the `IPC_CALL` system call grants both reading and writing). An
/// `interruptible` call fails with `-EINTR` when an upcall arrives; it is then
/// withdrawn, so the server can no longer reach the buffer.
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

/// The slot of `client` if it has a call to the current task that the
/// current task has received and not yet replied to.
fn received_call(t: &Table, client: u64) -> Result<usize, i64> {
    let slot = t.live_slot(client).ok_or(-ESRCH)?;
    let c = &t.tasks[slot];
    if c.blocked_in(State::BlockedCall) && c.call.server == t.cur().tid && c.call.received {
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
            let c = &mut t.tasks[slot];
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
            Ok(slot) => (t.tasks[slot].call, t.tasks[slot].pml4, t.cur().pml4),
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

/// Registers the current, privileged task as service `id` (the `svc_register` system call).
///
/// Registering as [`svc::PROC`] makes it the task manager.
pub(crate) fn svc_register(id: u64) -> i64 {
    if id == 0 || id >= svc::MAX {
        return -EINVAL;
    }
    let mut t = table();
    if !t.cur().privileged {
        return -EPERM;
    }
    let me = t.cur().tid;
    let holder = t.services[id as usize];
    if holder != 0 && holder != me && t.live_slot(holder).is_some() {
        return -EBUSY;
    }
    t.services[id as usize] = me;
    for p in t.tasks.iter_mut() {
        p.wake_from(State::BlockedService); // They retry their lookup.
    }
    0
}

/// Returns the task providing service `id` (the `svc_lookup` system call).
pub(crate) fn svc_lookup(id: u64) -> i64 {
    if id == 0 || id >= svc::MAX {
        return -EINVAL;
    }
    let t = table();
    let tid = t.services[id as usize];
    match t.live_slot(tid) {
        Some(_) => tid as i64,
        None => -ESRCH,
    }
}

/// Returns the task providing service `id`, waiting for it to register (the
/// `svc_wait` system call).
///
/// `interruptible`: whether an upcall ends the wait with `-EINTR`.
fn wait_for_service(id: u64, interruptible: bool) -> i64 {
    loop {
        let tid = svc_lookup(id);
        if tid != -ESRCH {
            return tid;
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

/// The `svc_wait` system call: [`svc_lookup`], but waits for the service.
pub(crate) fn svc_wait(id: u64) -> i64 {
    wait_for_service(id, true)
}

/// Forwards `read(0)` / `write(1|2)` to the console server as an `ipc_call`.
///
/// The call lets the server only write / only read the caller's buffer.
/// During boot the console may not have registered yet: wait for it.
///
/// A `read` may wait for a long time, for someone to type a line, so an
/// upcall interrupts it; a `write` always completes.
pub(crate) fn console_call(op: u64, buf: u64, len: u64, access: Access) -> i64 {
    let interruptible = op == haisuli_abi::console::OP_READ;
    match wait_for_service(svc::CONSOLE, interruptible) {
        // The console server itself must drive the hardware directly (a call
        // to itself fails with EDEADLK).
        tid if tid > 0 => ipc_call(tid as u64, op, buf, len, access, interruptible),
        e => e,
    }
}
