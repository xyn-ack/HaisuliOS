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
//! All scheduler state lives in one `Table` behind a [`Lock`]. Functions take it for short,
//! explicit scopes and always release it before switching context or idling.

use crate::arch::entry::TrapFrame;
use crate::arch::multiboot::{self, Module};
use crate::arch::{context, cpu};
use crate::sync::{Guard, Lock};
use crate::{elf, gdt, irq, syscall, vmm};
use haisuli_abi::errno::*;
use haisuli_abi::{svc, IO_PORTS, KMSG_IRQ, PID_KERNEL};

pub(crate) const MAX_PROCS: usize = 16;
const MBOX_SIZE: usize = 8;
const USER_STACK_PAGES: u64 = 8;
const NAME_LEN: usize = 16;
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
    /// Set by `ipc_reply` (or to -EPIPE if the server dies).
    result: i64,
}

struct Process {
    pid: u64,
    ppid: u64,
    state: State,
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
        state: State::Unused,
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
        },
    };

    fn alive(&self) -> bool {
        !matches!(self.state, State::Unused | State::Zombie)
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
            self.procs[slot].state = State::Ready;
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
/// boot ramdisk: it is mapped read-only into init at `RAMDISK_BASE`, and init
/// receives its address and size in `rdi` / `rsi`.
pub(crate) fn spawn_boot_modules() {
    let ramdisk = multiboot::modules().find(|m| !elf::is_elf(m.data));
    let mut started = 0;
    for m in multiboot::modules().filter(|m| elf::is_elf(m.data)) {
        let rd = if started == 0 {
            ramdisk.map_or(&[][..], |r| r.data)
        } else {
            &[]
        };
        match spawn(&m, rd) {
            Ok(pid) => {
                crate::println!(
                    "[kernel] started pid {} ({}): {} KiB image{}",
                    pid,
                    Name::new(m.name()),
                    m.data.len().div_ceil(1024),
                    if rd.is_empty() { "" } else { " + ramdisk" }
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

fn spawn(m: &Module, ramdisk: &[u8]) -> Result<u64, &'static str> {
    let slot = table().free_slot().ok_or("process table full")?;
    let pml4 = vmm::new_address_space().ok_or("out of memory")?;
    let setup = || -> Result<TrapFrame, &'static str> {
        let entry = elf::load(pml4, m.data)?;
        let stack = USER_STACK_PAGES * 4096;
        let stack_flags = vmm::USER | vmm::WRITABLE | vmm::NX;
        vmm::map_init(pml4, vmm::USER_STACK_TOP - stack, stack, &[], stack_flags)
            .map_err(|_| "out of memory")?;
        let mut frame = TrapFrame {
            rip: entry,
            cs: gdt::USER_CS,
            rflags: 0x202, // IF set (reserved bit 1 is always 1).
            rsp: vmm::USER_STACK_TOP,
            ss: gdt::USER_DS,
            ..TrapFrame::default()
        };
        if !ramdisk.is_empty() {
            let len = ramdisk.len() as u64;
            vmm::map_init(pml4, vmm::RAMDISK_BASE, len, ramdisk, vmm::USER | vmm::NX)
                .map_err(|_| "out of memory")?;
            frame.rdi = vmm::RAMDISK_BASE;
            frame.rsi = len;
        }
        Ok(frame)
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
        state: State::Ready,
        pml4,
        privileged: true,
        name: Name::new(m.name()),
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

/// Terminate the current process. `wstatus` uses the POSIX wait-status
/// encoding: (exit_code << 8) for normal exit, or a signal number.
pub(crate) fn exit_current(wstatus: i32) -> ! {
    let (pid, name, pml4) = {
        let mut t = table();
        let p = t.cur_mut();
        let pml4 = core::mem::replace(&mut p.pml4, 0);
        p.wstatus = wstatus;
        p.state = State::Zombie;
        let (pid, ppid, name) = (p.pid, p.ppid, p.name);
        release_server_state(&mut t, pid);
        for p in t.procs.iter_mut() {
            if p.pid == ppid && p.state == State::BlockedWait {
                p.state = State::Ready;
            }
        }
        (pid, name, pml4)
    };
    irq::release_all(pid);
    crate::println!(
        "[kernel] pid {} ({}) exited (wait status {:#x})",
        pid,
        name,
        wstatus
    );
    vmm::switch(vmm::kernel_pml4()); // Never free the active page tables.
    vmm::destroy_user_space(pml4);
    if pid == INIT_PID {
        crate::println!("[kernel] init exited - shutting down");
        cpu::qemu_exit(0);
        cpu::halt();
    }
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
            p.state = State::Ready;
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
        (t.free_slot(), (p.pid, p.pml4, p.mmap_next, p.name))
    };
    let Some(slot) = slot else {
        return -EAGAIN;
    };
    let (ppid, parent_pml4, mmap_next, name) = parent;
    let Some(pml4) = vmm::clone_user_space(parent_pml4) else {
        return -ENOMEM;
    };
    let mut child_frame = *frame;
    child_frame.rax = 0;
    context::init(slot, &child_frame);
    let pid = slot as u64 + 1;
    table().procs[slot] = Process {
        pid,
        ppid,
        state: State::Ready,
        pml4,
        mmap_next,
        name,
        ..Process::EMPTY
    };
    pid as i64
}

/// Waits for any child to exit and reaps it (the `wait4(-1, &status, ...)` system call).
pub(crate) fn wait(status_ptr: u64) -> i64 {
    loop {
        let mut t = table();
        let (me, pml4) = (t.cur().pid, t.cur().pml4);
        let mut have_child = false;
        for p in t.procs.iter_mut() {
            if p.state == State::Unused || p.ppid != me {
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
        t.cur_mut().state = State::BlockedWait;
        drop(t);
        schedule();
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
    let mut flags = vmm::USER;
    if prot & 2 != 0 {
        flags |= vmm::WRITABLE; // `PROT_WRITE`.
    }
    if prot & 4 == 0 {
        flags |= vmm::NX; // No `PROT_EXEC`.
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
/// buffer (the `IPC_CALL` system call grants both reading and writing).
pub(crate) fn ipc_call(dest: u64, op: u64, buf: u64, len: u64, access: Access) -> i64 {
    {
        let mut t = table();
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
            c.state = State::Ready;
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
pub(crate) fn console_call(op: u64, buf: u64, len: u64, access: Access) -> i64 {
    loop {
        let pid = svc_lookup(svc::CONSOLE);
        if pid > 0 {
            // The console server itself must drive the hardware directly
            // (a call to itself fails with EDEADLK).
            return ipc_call(pid as u64, op, buf, len, access);
        }
        table().cur_mut().state = State::BlockedService;
        schedule();
    }
}
