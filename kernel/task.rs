// SPDX-License-Identifier: GPL-2.0

//! Tasks and threads.
//!
//! Tasks, their threads, the scheduler of every CPU and inter-process communication.
//!
//! A *task* is a private address space with its file descriptors, mailbox and privileges; a
//! *thread* is an execution context in it (a kernel stack and saved registers, see
//! [`crate::arch::context`]). A task starts with one thread, its *leader*, whose id is also the
//! task's id; `clone` adds threads, `exit` ends one, and the task ends when its last thread does
//! (or when it is killed, faults or calls `task_exit`: then all of its threads end). All
//! user-to-kernel transitions save the user registers (a [`UserContext`]) at the top of the
//! thread's kernel stack.
//!
//! Every CPU runs the same scheduler over all threads: a thread is *runnable* if it is ready,
//! on no CPU, and its task is alive and not stopped; each CPU takes the next runnable thread after
//! the one it ran last (round robin). A thread runs until it blocks or yields, or until the
//! periodic tick finds that it has had its quantum while another thread waits: then it is
//! preempted on its way back to user mode. A CPU with nothing to run idles in its own context
//! ([`idle`]) until an interrupt: its tick, a device IRQ (which reaches the boot CPU only), or a
//! *reschedule IPI*, which other CPUs send when they make a thread runnable, or when a thread
//! running there must stop, end or enter an upcall.
//!
//! A task never vanishes from under a CPU. Ending it only marks it *dying*: its threads that are
//! on no CPU end at once, the others are marked killed and end themselves the next time they
//! return to user mode or block (their CPU is interrupted so that this happens soon). Its address
//! space and file descriptors are freed ([`reap`]) only once none of its threads is on a CPU any
//! more, and no other task's call is copying to or from its memory (a *pin*, see
//! [`ipc_transfer`]). A slot is not reused before that.
//!
//! IPC comes in two flavours:
//!
//! - Asynchronous notifications ([`ipc_send`]): two words into a small per-task mailbox; the
//!   kernel uses the same path for IRQs.
//! - Synchronous calls ([`ipc_call`]): the calling thread blocks until the server replies. While
//!   the call is pending, the server may copy data from and to the client's buffer (`ipc_read` /
//!   `ipc_write`), as far as the call's [`Access`] allows. So requests of any size need no kernel
//!   buffering, and a server can defer a reply (e.g. a console `read` until a line has been typed)
//!   without blocking itself. Every thread has at most one call outstanding, so callers simply
//!   queue in the thread table. Calls are addressed to tasks; any thread of the server may
//!   receive them, and replies go to the calling thread.
//!
//! The kernel knows nothing about POSIX processes. Process ids, parents and children, exit
//! statuses, process groups, sessions and signals are the business of the process server, which
//! the kernel knows only as the *task manager*: the task registered as service [`svc::PROC`]. It
//! alone may use the task control calls, the mechanisms that processes and signals are built on:
//! copying a task ([`task_fork`]), ending ([`task_kill`]), stopping and continuing one, making one
//! enter a handler ([`task_upcall`]), and running a program from the file system, in a task
//! ([`task_exec`]) or a new one ([`task_spawn`]). A task that ends by itself (with
//! [`end_current`], or when its last thread exits) is reported to it with a kernel notification.
//!
//! All scheduler state lives in one `Table` behind a [`Lock`]. Functions take it for short,
//! explicit scopes and always release it before switching context or idling.

use crate::arch::percpu::{self, MAX_CPUS};
use crate::arch::upcall::{Frame, SavedContext};
use crate::arch::{self, context, CpuInterrupts, IoPermissions, IoPorts, Processor};
use crate::arch::{UserContext, UserRegs};
use crate::platform::{Board, BootInfo, Machine, Module};
use crate::program::{self, Args};
use crate::sync::{Guard, Lock};
use crate::upcall::{is_user_address, Upcall};
use crate::vmm::AddressSpace;
use crate::{elf, fs, irq, vmm};
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use haisuli_abi::clone::THREAD_FLAGS;
use haisuli_abi::errno::*;
use haisuli_abi::exec::{self, ARG_MAX};
use haisuli_abi::fcntl::{AT_FDCWD, F_DUPFD, F_GETFD, OPEN_MAX};
use haisuli_abi::{svc, task_state, upcall, TaskInfo, TASK_INFO_LEN};
use haisuli_abi::{KMSG_IRQ, KMSG_TASK_EXIT, KMSG_TASK_FAULT, PID_KERNEL};

/// Thread slots: every thread of every task. A task's id is its leader's slot + 1.
pub(crate) const MAX_THREADS: usize = 64;
/// The most tasks at once (each takes the slot of its leader thread).
pub(crate) const MAX_TASKS: usize = 16;
const MBOX_SIZE: usize = 8;
const NAME_LEN: usize = 16;
/// A file descriptor that is not open.
const NO_FD: u8 = u8::MAX;
/// Descriptors per task.
const FDS: usize = OPEN_MAX as usize;
/// [`Thread::cpu`] of a thread that is on no CPU.
const NO_CPU: usize = usize::MAX;
/// [`Cpu::current`] of a CPU that idles.
const IDLE: usize = usize::MAX;
/// Ticks a thread may run before another runnable thread may take its CPU.
const QUANTUM_TICKS: u32 = 2;
/// Ticks after which an idle CPU looks at the scheduler, even if nobody told it to.
const IDLE_CHECK_TICKS: u32 = 10;

// Per-CPU hints that let the frequent paths (the tick, the return from an interrupt, an idle
// CPU waking up) skip the scheduler's lock when they have nothing to do: the lock is shared by
// every CPU, and the fewer CPUs wait for it, the better it works. They are only hints: whoever
// acts on one takes the lock and looks at the real state.

/// Whether the CPU runs a thread (set by [`pick`]).
static BUSY: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// Ticks since the CPU switched threads (or last looked, while idle).
static TICKS: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
/// Something may concern the thread the CPU runs (it must end, stop or enter an upcall, or
/// give way): look on the way back to user mode.
static ATTENTION: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// An idle CPU was told that there may be work.
static KICKED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// The thread running on the CPU made another one runnable and did not wake an idle CPU for it
/// yet (see [`Table::make_ready_later`]).
static KICK_LATER: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// The CPUs (one bit each) that this CPU is to interrupt once it lets go of the scheduler's
/// lock (see [`TableGuard`]).
static IPIS: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
const _: () = assert!(MAX_CPUS <= 32, "IPI sets are u32 bit masks");
/// Tasks that are dying, for [`reap`] to look at.
static DYING: AtomicUsize = AtomicUsize::new(0);

/// What a thread is doing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// The slot is free.
    Unused,
    /// Reserved for a task being created (`task_fork`, `task_spawn`).
    New,
    /// Running (see [`Thread::cpu`]) or ready to.
    Ready,
    BlockedRecv,
    /// In `ipc_call`, waiting for the server's reply.
    BlockedCall,
    /// Waiting for a well-known service to register.
    BlockedService,
    /// Ended. Its context may still be on its way off its CPU: the slot is free once
    /// [`context::on_cpu`] says it is not.
    Exited,
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

/// The outstanding `ipc_call` of a thread in `State::BlockedCall`.
#[derive(Clone, Copy)]
struct Call {
    /// The server's task id.
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

/// A thread: an execution context in a task.
#[derive(Clone, Copy)]
struct Thread {
    state: State,
    /// The slot of its task.
    task: usize,
    /// The CPU running it, or [`NO_CPU`].
    cpu: usize,
    /// Its task is ending: the thread must end as soon as it can (it was running then).
    killed: bool,
    /// `task_exec` waits for this thread, which it ended, to leave its CPU: the slot is not
    /// free before it has seen that.
    held: bool,
    /// An upcall to enter on the way back to user mode.
    upcall: Option<Upcall>,
    call: Call,
}

impl Thread {
    const EMPTY: Thread = Thread {
        state: State::Unused,
        task: 0,
        cpu: NO_CPU,
        killed: false,
        held: false,
        upcall: None,
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

    /// Whether the thread exists and has not ended.
    fn live(&self) -> bool {
        matches!(
            self.state,
            State::Ready | State::BlockedRecv | State::BlockedCall | State::BlockedService
        )
    }

    /// Whether the thread sleeps in a wait that an upcall interrupts.
    fn interruptible(&self) -> bool {
        match self.state {
            State::BlockedRecv | State::BlockedService => true,
            State::BlockedCall => self.call.interruptible,
            _ => false,
        }
    }

    /// Wakes the thread if it is blocked in state `blocked`.
    fn wake_from(&mut self, blocked: State) {
        if self.state == blocked {
            self.state = State::Ready;
        }
    }
}

/// Whether a task slot is in use.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Life {
    Unused,
    /// Reserved for a task being created.
    New,
    Alive,
    /// Ended, but a CPU may still be using it (see the module documentation).
    Dying,
}

struct Task {
    life: Life,
    /// The task id: its leader's thread id.
    id: u64,
    /// Stopped by the task manager, until it continues the task: its threads keep their state
    /// but do not run.
    stopped: bool,
    /// Threads that have not ended.
    threads: usize,
    /// Calls of other tasks that are copying from or to this task's memory.
    pins: usize,
    /// File descriptors: the open file description each refers to ([`fs`]),
    /// or [`NO_FD`].
    fds: [u8; FDS],
    space: AddressSpace,
    mmap_next: u64,
    mbox: [Msg; MBOX_SIZE],
    mb_head: usize,
    mb_len: usize,
    name: Name,
    /// Whether the task may be granted devices, bind IRQs and register services.
    ///
    /// Boot modules may; a copy made with `task_fork` may not.
    privileged: bool,
    /// The I/O ports that devices granted to the task have (on an architecture with ports).
    io: IoPorts,
    /// IRQ lines that fired and have not been delivered by `ipc_recv` yet, one bit each.
    irq_pending: u64,
}
const _: () = assert!(irq::LINES <= 64, "pending IRQ lines are u64 bit masks");

impl Task {
    const EMPTY: Task = Task {
        life: Life::Unused,
        id: 0,
        stopped: false,
        threads: 0,
        pins: 0,
        fds: [NO_FD; FDS],
        space: AddressSpace::NONE,
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
        io: IoPorts::NONE,
        irq_pending: 0,
    };

    fn alive(&self) -> bool {
        self.life == Life::Alive
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

/// What the scheduler knows about a CPU.
#[derive(Clone, Copy)]
struct Cpu {
    /// Whether the CPU runs the scheduler.
    online: bool,
    /// The thread slot it runs, or [`IDLE`].
    current: usize,
    /// The thread slot it ran last: the round robin goes on after it.
    last: usize,
    /// Whether it idles with nothing to do (and may be halted): it needs an IPI to notice work.
    idle: bool,
    /// The current thread has had its quantum and another one waits: switch on the way back to
    /// user mode.
    resched: bool,
}

impl Cpu {
    const OFFLINE: Cpu = Cpu {
        online: false,
        current: IDLE,
        // So that the first scan starts at slot 0: boot tasks start in order.
        last: MAX_THREADS - 1,
        idle: false,
        resched: false,
    };
}

struct Table {
    threads: [Thread; MAX_THREADS],
    /// Tasks, in the slot of their leader thread.
    tasks: [Task; MAX_THREADS],
    cpus: [Cpu; MAX_CPUS],
    call_seq: u64,
    /// Well-known services: task registered for each service id (0 = none).
    services: [u64; svc::MAX as usize],
    /// Slots of tasks that ended themselves, not yet reported to the task manager.
    ///
    /// Such a slot is not reused before the report, so the id in it stays unambiguous.
    ended: u64,
    /// Slots of tasks that faulted, not yet reported to the task manager.
    faulted: u64,
    /// Whether the CPUs may run threads yet (see [`start`]).
    started: bool,
    /// Where the search for a free slot starts: after the last one taken. Ids are handed out
    /// round robin, so an id that was just freed is the last to be used again.
    next_slot: usize,
}

impl Table {
    /// The thread slot running on this CPU.
    ///
    /// # Panics
    ///
    /// Panics on a CPU that idles: only threads make system calls.
    fn me(&self) -> usize {
        let slot = self.cpus[percpu::index()].current;
        assert!(slot != IDLE, "no thread runs on this CPU");
        slot
    }

    /// The slot of the running thread's task.
    fn my_task(&self) -> usize {
        self.threads[self.me()].task
    }

    /// The running thread's task.
    fn cur(&self) -> &Task {
        &self.tasks[self.my_task()]
    }

    fn cur_mut(&mut self) -> &mut Task {
        let slot = self.my_task();
        &mut self.tasks[slot]
    }

    /// The slot of a live task. tid = slot + 1.
    fn live_slot(&self, tid: u64) -> Option<usize> {
        if tid == 0 || tid > MAX_THREADS as u64 {
            return None;
        }
        let slot = (tid - 1) as usize;
        self.tasks[slot].alive().then_some(slot)
    }

    /// The slot of live thread `id`, of a live task. id = slot + 1.
    fn thread_slot(&self, id: u64) -> Option<usize> {
        if id == 0 || id > MAX_THREADS as u64 {
            return None;
        }
        let slot = (id - 1) as usize;
        let th = &self.threads[slot];
        (th.live() && self.tasks[th.task].alive()).then_some(slot)
    }

    /// A slot for a new thread: no thread, no task (a task keeps its leader's slot until it is
    /// gone), and no unreported end.
    fn free_slot(&self) -> Option<usize> {
        let unreported = self.ended | self.faulted;
        (0..MAX_THREADS)
            .map(|off| (self.next_slot + off) % MAX_THREADS)
            .find(|&i| {
                let th = &self.threads[i];
                let free = match th.state {
                    State::Unused => true,
                    State::Exited => !th.held && !context::on_cpu(i),
                    _ => false,
                };
                free && self.tasks[i].life == Life::Unused && unreported & (1 << i) == 0
            })
    }

    /// A slot for a new task, if there is room for one.
    fn new_task_slot(&self) -> Option<usize> {
        let tasks = self.tasks.iter().filter(|p| p.life != Life::Unused).count();
        if tasks < MAX_TASKS {
            self.free_slot()
        } else {
            None
        }
    }

    /// Reserves `slot` for a task being created.
    fn reserve(&mut self, slot: usize) {
        self.threads[slot] = Thread {
            state: State::New,
            ..Thread::EMPTY
        };
        self.tasks[slot].life = Life::New;
        self.next_slot = slot + 1;
    }

    /// Gives up a slot that [`reserve`](Self::reserve) took.
    fn unreserve(&mut self, slot: usize) {
        self.threads[slot] = Thread::EMPTY;
        self.tasks[slot].life = Life::Unused;
    }

    /// The live threads of the task in `slot`.
    fn threads_of(&self, slot: usize) -> impl Iterator<Item = usize> + '_ {
        (0..MAX_THREADS).filter(move |&i| self.threads[i].task == slot && self.threads[i].live())
    }

    /// The live threads of the task in `slot`, as a set (see [`slots`]).
    fn thread_set(&self, slot: usize) -> u64 {
        self.threads_of(slot).fold(0, |set, i| set | 1 << i)
    }

    /// Wakes a thread of the task in `slot` that waits in `ipc_recv`, and an idle CPU for it.
    fn wake_receiver(&mut self, slot: usize) {
        if self.wake_receiver_here(slot) {
            self.kick_idle();
        }
    }

    /// Wakes a thread of the task in `slot` that waits in `ipc_recv`, but no other CPU: the
    /// caller is about to block, and then its CPU takes it (a direct hand-off, as from a
    /// client to its server). Returns whether it woke one.
    fn wake_receiver_here(&mut self, slot: usize) -> bool {
        let receiver = (0..MAX_THREADS)
            .find(|&i| self.threads[i].task == slot && self.threads[i].state == State::BlockedRecv);
        if let Some(i) = receiver {
            self.threads[i].state = State::Ready;
        }
        receiver.is_some()
    }

    /// Makes thread `slot` ready (it got what it waited for).
    ///
    /// A thread of a stopped task stays stopped: it only runs once the task is continued, so
    /// that a reply or a message cannot un-stop it.
    fn make_ready(&mut self, slot: usize) {
        self.threads[slot].state = State::Ready;
        self.kick_idle();
    }

    /// Makes thread `slot` ready, but wakes an idle CPU for it only if the running thread does
    /// not give up this CPU soon.
    ///
    /// For what a thread makes runnable just before it usually blocks: a server's reply (before
    /// it waits for the next request), the program after `execve`, a new task or thread. Then
    /// this CPU runs it next, and no other CPU is woken up just to find it gone, which costs
    /// both of them (they contend for the scheduler's lock). If the running thread goes on
    /// instead, the next tick wakes an idle CPU (see [`tick`]).
    fn make_ready_later(&mut self, slot: usize) {
        self.threads[slot].state = State::Ready;
        self.kick_later();
    }

    /// Wakes idle CPUs for the runnable threads at the next tick, unless this CPU schedules
    /// before.
    fn kick_later(&mut self) {
        if self.cpus[percpu::index()].current == IDLE {
            self.kick_idle(); // An interrupt handler on an idle CPU: nothing runs here.
        } else {
            KICK_LATER[percpu::index()].store(true, Ordering::Relaxed);
        }
    }

    /// Slot of the task manager (the task registered as [`svc::PROC`]).
    fn manager_slot(&self) -> Option<usize> {
        self.live_slot(self.services[svc::PROC as usize])
    }

    /// Whether the running thread belongs to the task manager.
    fn is_manager(&self) -> bool {
        self.manager_slot() == Some(self.my_task())
    }

    /// The slot of task `tid`, if the running task is the task manager and may control it (any
    /// live task but itself).
    fn controlled_slot(&self, tid: u64) -> Result<usize, i64> {
        if !self.is_manager() {
            return Err(-EPERM);
        }
        match self.live_slot(tid) {
            Some(slot) if slot == self.my_task() => Err(-EINVAL),
            Some(slot) => Ok(slot),
            None => Err(-ESRCH),
        }
    }

    /// The slot of thread `client`, if the running task is the task manager and `client` is
    /// blocked in a call to it that it has received, from a task it may control.
    fn controlled_client(&self, client: u64) -> Result<usize, i64> {
        let slot = self.received_call(client)?;
        self.controlled_slot(self.tasks[self.threads[slot].task].id)?;
        Ok(slot)
    }

    /// Whether thread `slot` may run now on a CPU that looks for work.
    fn runnable(&self, slot: usize) -> bool {
        let th = &self.threads[slot];
        let task = &self.tasks[th.task];
        th.state == State::Ready
            && th.cpu == NO_CPU
            && !th.killed
            && task.alive()
            && !task.stopped
            && self.started
    }

    /// Interrupts CPU `cpu`, so that it looks at the scheduler again: an idle one for work,
    /// a busy one for what concerns its thread (unless it is this one, which will anyway).
    fn kick(&self, cpu: usize) {
        let c = &self.cpus[cpu];
        KICKED[cpu].store(true, Ordering::Release);
        ATTENTION[cpu].store(true, Ordering::Release);
        let me = percpu::index();
        if cpu != me && c.online {
            // Sent once the lock is free: it is a device access, which can be slow (emulated,
            // it may wait for the emulator), and every CPU waits for whoever holds the lock.
            IPIS[me].fetch_or(1 << cpu, Ordering::Relaxed);
        }
    }

    /// Wakes idle CPUs, one for each runnable thread that no CPU runs. This CPU counts too, if
    /// it idles (an interrupt made a thread runnable): it looks once the interrupt is handled.
    fn kick_idle(&mut self) {
        let mut waiting = (0..MAX_THREADS).filter(|&i| self.runnable(i)).count();
        let me = percpu::index();
        let cpus = core::iter::once(me).chain((0..MAX_CPUS).filter(|&c| c != me));
        for cpu in cpus {
            if waiting == 0 {
                break;
            }
            let c = &self.cpus[cpu];
            if c.online && c.idle && c.current == IDLE {
                self.cpus[cpu].idle = false;
                self.kick(cpu);
                waiting -= 1;
            }
        }
    }

    /// The received call from thread `client` to the running task, not yet replied to: the
    /// client's slot.
    fn received_call(&self, client: u64) -> Result<usize, i64> {
        let slot = self.thread_slot(client).ok_or(-ESRCH)?;
        let c = &self.threads[slot];
        if c.state == State::BlockedCall && c.call.server == self.cur().id && c.call.received {
            Ok(slot)
        } else {
            Err(-EINVAL)
        }
    }

    /// The online CPUs, one bit each.
    fn online_mask(&self) -> u64 {
        (0..MAX_CPUS)
            .filter(|&c| self.cpus[c].online)
            .fold(0, |mask, c| mask | 1 << c)
    }
}

/// The thread slots in a set of them (one bit each).
fn slots(set: u64) -> impl Iterator<Item = usize> {
    (0..MAX_THREADS).filter(move |&i| set & (1 << i) != 0)
}
const _: () = assert!(MAX_THREADS <= 64, "thread sets are u64 bit masks");

static TABLE: Lock<Table> = Lock::new(Table {
    threads: [Thread::EMPTY; MAX_THREADS],
    tasks: [Task::EMPTY; MAX_THREADS],
    cpus: [Cpu::OFFLINE; MAX_CPUS],
    call_seq: 0,
    services: [0; svc::MAX as usize],
    ended: 0,
    faulted: 0,
    started: false,
    next_slot: 0,
});

/// The scheduler's lock, held: on release, it sends the IPIs that [`Table::kick`] asked for.
struct TableGuard(Option<Guard<'static, Table>>);

impl core::ops::Deref for TableGuard {
    type Target = Table;
    fn deref(&self) -> &Table {
        self.0
            .as_ref()
            .expect("the table guard is held until dropped")
    }
}

impl core::ops::DerefMut for TableGuard {
    fn deref_mut(&mut self) -> &mut Table {
        self.0
            .as_mut()
            .expect("the table guard is held until dropped")
    }
}

impl Drop for TableGuard {
    fn drop(&mut self) {
        drop(self.0.take());
        let cpus = IPIS[percpu::index()].swap(0, Ordering::Relaxed);
        for cpu in (0..MAX_CPUS).filter(|&c| cpus & (1 << c) != 0) {
            arch::Cpu::send_reschedule(cpu);
        }
    }
}

fn table() -> TableGuard {
    TableGuard(Some(TABLE.lock()))
}

/// The running task's id.
pub(crate) fn current_tid() -> u64 {
    table().cur().id
}
pub(crate) fn current_space() -> AddressSpace {
    table().cur().space
}
pub(crate) fn current_privileged() -> bool {
    table().cur().privileged
}
pub(crate) fn is_alive(tid: u64) -> bool {
    table().live_slot(tid).is_some()
}

// ---- CPUs ---------------------------------------------------------------------

/// Makes this CPU one that runs threads (once [`Processor::init_cpu`] has set it up).
pub(crate) fn cpu_online() {
    let mut t = table();
    t.cpus[percpu::index()].online = true;
}

/// How many CPUs run threads.
pub(crate) fn cpus_online() -> usize {
    table().online_mask().count_ones() as usize
}

// ---- Creating tasks -------------------------------------------------------------

/// Starts every ELF boot module as a privileged task, in module order.
///
/// The first becomes task 1 (init). A module that is not an ELF file is the
/// boot ramdisk, a ustar archive: it is unpacked into the file system
/// ([`fs::init`]). (The process server adopts the boot tasks as processes,
/// with their task ids as pids.)
pub(crate) fn spawn_boot_modules() {
    let ramdisk = Board::modules().find(|m| !elf::is_elf(m.data));
    fs::init(ramdisk.map_or(&[][..], |r| r.data));
    let mut started = 0;
    for m in Board::modules().filter(|m| elf::is_elf(m.data)) {
        match spawn(&m) {
            Ok(tid) => {
                crate::println!(
                    "[kernel] started task {} ({}): {} KiB image",
                    tid,
                    Name::new(m.name),
                    m.data.len().div_ceil(1024),
                );
                started += 1;
            }
            Err((_, why)) => crate::println!(
                "[kernel] cannot start boot module \"{}\": {}",
                Name::new(m.name),
                why
            ),
        }
    }
    if let Some(r) = ramdisk {
        crate::println!(
            "[kernel] ramdisk \"{}\": {} KiB",
            Name::new(r.name),
            r.data.len().div_ceil(1024)
        );
    }
    if started == 0 {
        panic!("no runnable boot modules (boot with the modules listed in the README)");
    }
}

/// Starts boot module `m`, with its name as its only argument.
fn spawn(m: &Module) -> Result<u64, program::Error> {
    let name = core::str::from_utf8(m.name).unwrap_or("?");
    let mut block = [0u8; 64];
    let len = exec::build(name, &[name], &[], &mut block).map_err(|e| (e, "name too long"))?;
    let args = Args::parse(&block[..len])?;
    let slot = {
        let mut t = table();
        let slot = t.new_task_slot().ok_or((-EAGAIN, "task table full"))?;
        t.reserve(slot);
        slot
    };
    match program::load(m.data, &args) {
        Ok((space, frame)) => {
            let fds = console_fds();
            Ok(install(
                &mut table(),
                slot,
                space,
                &frame,
                Name::new(args.name()),
                true,
                fds,
            ))
        }
        Err(e) => {
            table().unreserve(slot);
            Err(e)
        }
    }
}

/// Descriptors 0, 1 and 2 on the terminal, and nothing else.
fn console_fds() -> [u8; FDS] {
    let mut fds = [NO_FD; FDS];
    fds[..3].fill(fs::CONSOLE as u8);
    fds
}

/// Makes the reserved `slot` a new task running `space` from `frame`, with one thread; returns
/// its id. The task owns the references of `fds`.
fn install(
    t: &mut Table,
    slot: usize,
    space: AddressSpace,
    frame: &UserContext,
    name: Name,
    privileged: bool,
    fds: [u8; FDS],
) -> u64 {
    context::init(slot, frame);
    let id = slot as u64 + 1;
    t.tasks[slot] = Task {
        life: Life::Alive,
        id,
        threads: 1,
        space,
        name,
        privileged,
        fds,
        ..Task::EMPTY
    };
    t.threads[slot] = Thread {
        state: State::Ready,
        task: slot,
        ..Thread::EMPTY
    };
    t.kick_later();
    id
}

// ---- Scheduling -------------------------------------------------------------------

/// Where a CPU goes next: a thread (with what it needs to run), or idling.
struct Next {
    slot: usize,
    space: AddressSpace,
    io: IoPorts,
}

/// Picks the next runnable thread for this CPU after the one it ran last, and makes it the
/// CPU's current thread; or idles.
fn pick(t: &mut Table) -> Next {
    let cpu = percpu::index();
    let start = t.cpus[cpu].last;
    let next = (1..=MAX_THREADS)
        .map(|off| (start + off) % MAX_THREADS)
        .find(|&i| t.runnable(i));
    let c = &mut t.cpus[cpu];
    c.resched = false;
    TICKS[cpu].store(0, Ordering::Relaxed);
    BUSY[cpu].store(next.is_some(), Ordering::Relaxed);
    let Some(slot) = next else {
        c.current = IDLE;
        return Next {
            slot: IDLE,
            space: AddressSpace::kernel(),
            io: IoPorts::NONE,
        };
    };
    c.current = slot;
    c.last = slot;
    c.idle = false;
    t.threads[slot].cpu = cpu;
    let task = &t.tasks[t.threads[slot].task];
    Next {
        slot,
        space: task.space,
        io: task.io,
    }
}

/// Switches this CPU to `next`: its address space, whatever else the CPU needs for the thread
/// ([`Processor::prepare_thread`]), then its context. Returns when the thread that calls it runs
/// again.
///
/// The address space changes first: once the old context is off this CPU (which is what lets
/// another CPU free the old address space, see [`reap`]), this CPU no longer uses it.
fn switch(next: Next) {
    vmm::switch(next.space);
    if next.slot == IDLE {
        context::switch_to(context::idle(percpu::index()));
    } else {
        arch::Cpu::prepare_thread(next.slot, &next.io);
        context::switch_to(context::thread(next.slot));
    }
}

/// Lets this CPU run another thread (round robin), or idle.
///
/// The running thread leaves its CPU: it runs again once it is runnable and a CPU picks it
/// (possibly another one), and then this returns. A thread whose task ended never returns.
pub(crate) fn schedule() {
    let next = {
        let mut t = table();
        let me = t.me();
        if t.threads[me].killed {
            drop(t);
            exit_current();
        }
        t.threads[me].cpu = NO_CPU;
        let next = pick(&mut t);
        let cpu = percpu::index();
        if next.slot == me {
            if KICK_LATER[cpu].swap(false, Ordering::Relaxed) {
                t.kick_idle();
            }
            return;
        }
        // What this CPU does not run (maybe the thread itself, if it is still runnable, and
        // what it made runnable), other CPUs may.
        KICK_LATER[cpu].store(false, Ordering::Relaxed);
        t.kick_idle();
        next
    };
    switch(next);
    reap();
}

/// Where CPUs go when they have nothing else to do, forever: every CPU calls this once, on the
/// stack it started on, which becomes its idle context.
///
/// The CPU runs threads as they become runnable and halts in between, until an interrupt (its
/// tick, an IRQ, or a reschedule IPI) brings news. If every CPU idles and no interrupt can make
/// progress possible any more, the machine powers off.
pub(crate) fn idle() -> ! {
    context::enter_idle();
    arch::Cpu::start_tick();
    let cpu = percpu::index();
    loop {
        reap();
        let next = {
            let mut t = table();
            let next = pick(&mut t);
            if next.slot == IDLE {
                t.cpus[cpu].idle = true;
                if t.started {
                    check_deadlock(&t);
                }
                None
            } else {
                Some(next)
            }
        };
        match next {
            Some(next) => switch(next),
            None => {
                // Until someone says there may be work (a kick, also from an interrupt handler
                // on this CPU), or it is time to look anyway.
                loop {
                    arch::Cpu::wait_for_interrupt();
                    if KICKED[cpu].swap(false, Ordering::Acquire)
                        || TICKS[cpu].load(Ordering::Relaxed) >= IDLE_CHECK_TICKS
                    {
                        TICKS[cpu].store(0, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
    }
}

/// Lets the CPUs run threads, then idles (the boot CPU's last step of booting).
pub(crate) fn start() -> ! {
    {
        let mut t = table();
        t.started = true;
        t.kick_idle();
    }
    idle()
}

/// Whether an interrupt can still make progress possible.
///
/// That is the case if some IRQ line's owner is waiting in `ipc_recv`: its
/// notification will wake it up.
fn irq_can_wake(t: &Table) -> bool {
    (0..irq::LINES).any(|line| {
        t.live_slot(irq::owner(line)).is_some_and(|s| {
            !t.tasks[s].stopped
                && t.threads_of(s)
                    .any(|i| t.threads[i].state == State::BlockedRecv)
        })
    })
}

/// Stops the machine if nothing can ever run again: every CPU idles, no thread is runnable and
/// no interrupt can change that.
fn check_deadlock(t: &Table) {
    let busy = t.cpus.iter().any(|c| c.online && c.current != IDLE);
    if busy || (0..MAX_THREADS).any(|i| t.runnable(i)) || irq_can_wake(t) {
        return;
    }
    if t.tasks.iter().any(Task::alive) {
        crate::println!("[kernel] deadlock: tasks blocked, none runnable");
    } else {
        crate::println!("[kernel] all tasks have ended - shutting down");
    }
    Board::power_off(0);
}

/// The scheduler's tick, on every CPU (from its [`CpuInterrupts::start_tick`] interrupt).
///
/// Once the running thread has had its quantum, and another thread waits for a CPU, it is
/// preempted on its way back to user mode (see [`return_to_user`]).
pub(crate) fn tick() {
    let cpu = percpu::index();
    let ticks = TICKS[cpu].fetch_add(1, Ordering::Relaxed) + 1;
    if KICK_LATER[cpu].swap(false, Ordering::Relaxed) {
        // The thread that made others runnable went on running: wake idle CPUs for them.
        table().kick_idle();
    }
    // Idle, or within the quantum: nothing to decide. After it, look once per quantum.
    if !BUSY[cpu].load(Ordering::Relaxed) || !ticks.is_multiple_of(QUANTUM_TICKS) {
        return;
    }
    let mut t = table();
    if t.cpus[cpu].current != IDLE && (0..MAX_THREADS).any(|i| t.runnable(i)) {
        t.cpus[cpu].resched = true;
        ATTENTION[cpu].store(true, Ordering::Release);
    }
}

pub(crate) fn yield_now() -> i64 {
    schedule();
    0
}

// ---- Ending threads and tasks ---------------------------------------------------------

/// Why a task ended by itself.
#[derive(Clone, Copy)]
pub(crate) enum Why {
    /// It called `TASK_EXIT`, or its last thread ended.
    Exit,
    /// It caused a CPU exception, or its upcall frame could not be used.
    Fault,
}

/// Ends the task in `slot`: it becomes dying, and every thread of it ends (see the module
/// documentation).
///
/// The calls it was serving fail, its services are dropped and its IRQs released at once. If
/// it ended by itself (`why`), the task manager is told.
fn end_task(t: &mut Table, slot: usize, why: Option<Why>) {
    if !t.tasks[slot].alive() {
        return;
    }
    t.tasks[slot].life = Life::Dying;
    DYING.fetch_add(1, Ordering::Relaxed);
    let id = t.tasks[slot].id;
    let me = percpu::index();
    for i in 0..MAX_THREADS {
        let th = t.threads[i];
        if th.task != slot || !th.live() {
            continue;
        }
        if th.cpu == NO_CPU {
            t.threads[i].state = State::Exited;
            t.threads[i].upcall = None;
            t.tasks[slot].threads -= 1;
        } else {
            t.threads[i].killed = true;
            if th.cpu != me {
                t.kick(th.cpu);
            }
        }
    }
    release_server_state(t, id);
    irq::release_all(id);
    // Ending the task manager itself dropped its registration: then there is nobody to tell.
    if let (Some(why), Some(manager)) = (why, t.manager_slot()) {
        match why {
            Why::Exit => t.ended |= 1 << slot,
            Why::Fault => t.faulted |= 1 << slot,
        }
        t.wake_receiver(manager);
    }
}

/// Ends the running thread. If it was the last thread of its task, the task ends too (and the
/// task manager is told: it ended by itself).
fn exit_current() -> ! {
    let next = {
        let mut t = table();
        let me = t.me();
        let slot = t.threads[me].task;
        let th = &mut t.threads[me];
        th.state = State::Exited;
        th.cpu = NO_CPU;
        th.killed = false;
        th.upcall = None;
        t.tasks[slot].threads -= 1;
        if t.tasks[slot].threads == 0 {
            end_task(&mut t, slot, Some(Why::Exit));
        }
        let next = pick(&mut t);
        KICK_LATER[percpu::index()].store(false, Ordering::Relaxed);
        t.kick_idle();
        next
    };
    switch(next);
    unreachable!("an exited thread was resumed");
}

/// What [`reap`] leaves for its caller to free outside the lock.
struct Dead {
    space: AddressSpace,
    fds: [u8; FDS],
}

/// Frees what dying tasks held, once nothing uses it any more (see the module documentation).
///
/// Called after every context switch and while idling, and by whatever drops the last use.
pub(crate) fn reap() {
    while DYING.load(Ordering::Relaxed) != 0 {
        let dead = {
            let mut t = table();
            let Some(slot) = (0..MAX_THREADS).find(|&slot| reapable(&t, slot)) else {
                return;
            };
            for th in t.threads.iter_mut() {
                if th.task == slot && th.state == State::Exited {
                    *th = Thread::EMPTY;
                }
            }
            let p = &mut t.tasks[slot];
            let dead = Dead {
                space: p.space,
                fds: p.fds,
            };
            *p = Task::EMPTY;
            DYING.fetch_sub(1, Ordering::Relaxed);
            dead
        };
        for &ofd in dead.fds.iter().filter(|&&fd| fd != NO_FD) {
            fs::release(ofd as usize);
        }
        vmm::destroy_user_space(dead.space);
    }
}

/// Whether the dying task in `slot` is no longer used by anything: no thread of it runs or is
/// still leaving a CPU, and no other task's call copies its memory.
fn reapable(t: &Table, slot: usize) -> bool {
    let p = &t.tasks[slot];
    p.life == Life::Dying
        && p.threads == 0
        && p.pins == 0
        && !(0..MAX_THREADS).any(|i| {
            t.threads[i].task == slot && t.threads[i].state == State::Exited && context::on_cpu(i)
        })
}

/// Ends the running task (every thread of it) and reports it to the task manager.
pub(crate) fn end_current(why: Why) -> ! {
    {
        let mut t = table();
        let slot = t.my_task();
        end_task(&mut t, slot, Some(why));
    }
    exit_current()
}

/// Ends the calling thread (the `exit` system call).
pub(crate) fn thread_exit() -> ! {
    exit_current()
}

/// Fails the pending calls and drops the service registrations of task `tid`, which is gone.
///
/// Its IRQ lines are released separately.
fn release_server_state(t: &mut Table, tid: u64) {
    for i in 0..MAX_THREADS {
        let th = &t.threads[i];
        if th.state == State::BlockedCall && th.call.server == tid {
            t.threads[i].call.result = -EPIPE;
            t.make_ready(i);
        }
    }
    for s in t.services.iter_mut() {
        if *s == tid {
            *s = 0;
        }
    }
}

// ---- Threads ----------------------------------------------------------------------

/// Starts a thread in the running task (the `clone` system call): a copy of the calling thread
/// `f`, which returns 0 on `stack`. Returns its id.
pub(crate) fn clone(f: &UserContext, flags: u64, stack: u64) -> i64 {
    if flags != THREAD_FLAGS || stack == 0 || !is_user_address(stack) {
        return -EINVAL;
    }
    let mut t = table();
    let task = t.my_task();
    if t.threads[t.me()].killed || !t.tasks[task].alive() {
        // The task is ending (the caller ends on its way out): a new thread would never run,
        // and the task never be freed.
        return -ESRCH;
    }
    let Some(slot) = t.free_slot() else {
        return -EAGAIN;
    };
    let mut frame = *f;
    frame.set_return(0); // What the new thread's `clone` gives back.
    frame.set_sp(stack);
    context::init(slot, &frame);
    t.next_slot = slot + 1;
    t.threads[slot] = Thread {
        state: State::Ready,
        task,
        ..Thread::EMPTY
    };
    t.tasks[task].threads += 1;
    t.kick_later();
    slot as i64 + 1
}

/// The calling thread's id (the `gettid` system call).
pub(crate) fn gettid() -> i64 {
    table().me() as i64 + 1
}

/// Stores the caller's CPU in `*cpu` and 0 in `*node` (the `getcpu` system call); null
/// pointers are skipped.
pub(crate) fn getcpu(cpu_out: u64, node_out: u64) -> i64 {
    let (space, cpu) = (current_space(), percpu::index() as u32);
    for (ptr, value) in [(cpu_out, cpu), (node_out, 0)] {
        if ptr != 0 && !vmm::copy_to_user(space, ptr, &value.to_ne_bytes()) {
            return -EFAULT;
        }
    }
    0
}

/// Writes the online CPUs to `mask[..len]` (the `sched_getaffinity` system call): every thread
/// may run on each of them.
pub(crate) fn sched_getaffinity(pid: u64, len: u64, mask: u64) -> i64 {
    if pid != 0 {
        return -ESRCH; // The kernel knows no pids: only the caller itself.
    }
    if len < 8 {
        return -EINVAL;
    }
    let (space, online) = {
        let t = table();
        (t.cur().space, t.online_mask())
    };
    if !vmm::copy_to_user(space, mask, &online.to_ne_bytes()) {
        return -EFAULT;
    }
    8
}

// ---- File descriptors ---------------------------------------------------------

/// A reference to an open file description, held while a system call uses it.
///
/// Another thread of the task may close the descriptor meanwhile: the description stays
/// until the reference is dropped, and cannot be reused (for another task's file, say) before.
pub(crate) struct Description(usize);

impl Description {
    /// The description's index in [`fs`].
    pub(crate) fn index(&self) -> usize {
        self.0
    }

    /// Hands the reference over to a file descriptor.
    fn into_fd_reference(self) -> usize {
        let ofd = self.0;
        core::mem::forget(self);
        ofd
    }
}

impl Drop for Description {
    fn drop(&mut self) {
        fs::release(self.0);
    }
}

/// The open file description that the caller's descriptor `fd` refers to.
pub(crate) fn description(fd: u64) -> Option<Description> {
    let t = table();
    match t.cur().fds.get(fd as usize) {
        Some(&ofd) if ofd != NO_FD => {
            fs::retain(ofd as usize);
            Some(Description(ofd as usize))
        }
        _ => None,
    }
}

/// Gives description `ofd` the caller's lowest free descriptor that is at
/// least `min`, handing over the reference the caller holds.
fn install_fd(ofd: usize, min: u64) -> i64 {
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

/// Opens a file (the `openat` system call), given the path the caller passed.
///
/// A relative path starts at directory `dirfd` (which must be open: `-EBADF`), or at the root
/// for [`AT_FDCWD`]; an absolute path ignores `dirfd`.
pub(crate) fn openat(dirfd: u64, path: &[u8], flags: u64) -> i64 {
    let dir = if path.starts_with(b"/") || dirfd == AT_FDCWD {
        None
    } else {
        match description(dirfd) {
            Some(dir) => Some(dir),
            None => return -EBADF,
        }
    };
    match fs::open(dir.as_ref().map(Description::index), path, flags) {
        Ok(ofd) => install_fd(ofd, 0),
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
    install_fd(ofd.into_fd_reference(), min)
}

/// Makes `newfd` refer to what `fd` refers to, closing it first (the `dup3` system call).
///
/// Unlike `dup2`, `fd` and `newfd` must differ, and `flags` must be 0: there is no `O_CLOEXEC`
/// yet.
pub(crate) fn dup3(fd: u64, newfd: u64, flags: u64) -> i64 {
    if fd == newfd || flags != 0 {
        return -EINVAL;
    }
    dup2(fd, newfd)
}

/// Makes `newfd` refer to what `fd` refers to, closing it first (the `dup2`
/// system call, and [`dup3`]).
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
    let ofd = ofd.into_fd_reference();
    let old = core::mem::replace(&mut table().cur_mut().fds[newfd as usize], ofd as u8);
    if old != NO_FD {
        fs::release(old as usize);
    }
    newfd as i64
}

/// The `fcntl` system call: only `F_DUPFD`, and `F_GETFD` (a descriptor has no flags yet).
pub(crate) fn fcntl(fd: u64, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => dup(fd, arg),
        F_GETFD => match description(fd) {
            Some(_) => 0,
            None => -EBADF,
        },
        _ => -EINVAL,
    }
}

// ---- Task control (the task manager's mechanisms) ------------------------------

/// Creates a copy of the task of thread `client` (the `task_fork` system call).
///
/// `client` must be blocked in a call to the caller, the task manager, that it
/// has received. The copy gets an eager (non-COW) copy of the address space,
/// one thread with `client`'s registers, whose pending call returns 0, and the file
/// descriptors, which share their open file descriptions. It gets no privileges, I/O
/// permissions, IRQs, messages or upcalls. Returns the new task's id.
pub(crate) fn task_fork(client: u64) -> i64 {
    let (parent, slot, parent_space, fds, name) = {
        let mut t = table();
        let cslot = match t.controlled_client(client) {
            Ok(slot) => slot,
            Err(e) => return e,
        };
        let parent = t.threads[cslot].task;
        let Some(slot) = t.new_task_slot() else {
            return -EAGAIN;
        };
        t.reserve(slot);
        let p = &mut t.tasks[parent];
        // Its memory must outlive the copy, even if the task ends meanwhile; and the copy's
        // descriptors are referenced now: another thread of the task may close them meanwhile.
        p.pins += 1;
        for &ofd in p.fds.iter().filter(|&&fd| fd != NO_FD) {
            fs::retain(ofd as usize);
        }
        (parent, slot, p.space, p.fds, p.name)
    };
    let space = vmm::clone_user_space(parent_space);
    let mut t = table();
    t.tasks[parent].pins -= 1;
    let failed = match (space, t.controlled_client(client)) {
        (None, _) => Some(-ENOMEM),
        (Some(_), Err(e)) => Some(e), // The task ended meanwhile.
        (Some(_), Ok(_)) => None,
    };
    if let Some(e) = failed {
        t.unreserve(slot);
        drop(t);
        for &ofd in fds.iter().filter(|&&fd| fd != NO_FD) {
            fs::release(ofd as usize);
        }
        if let Some(space) = space {
            vmm::destroy_user_space(space);
        }
        reap();
        return e;
    }
    let (Some(space), Ok(cslot)) = (space, t.controlled_client(client)) else {
        unreachable!("checked above");
    };
    let mut frame = context::user_frame(cslot);
    frame.set_return(0); // The result of the copy's call.
    let mmap_next = t.tasks[parent].mmap_next;
    let id = install(&mut t, slot, space, &frame, name, false, fds);
    t.tasks[slot].mmap_next = mmap_next;
    id as i64
}

/// Ends task `tid` at once (the `task_kill` system call).
///
/// Whatever its threads were blocked in is abandoned: a call one made is withdrawn, and
/// calls it was serving fail with `-EPIPE`. Threads running on other CPUs end as soon as
/// those notice.
pub(crate) fn task_kill(tid: u64) -> i64 {
    {
        let mut t = table();
        match t.controlled_slot(tid) {
            Ok(slot) => end_task(&mut t, slot, None),
            Err(e) => return e,
        }
    }
    reap();
    0
}

/// Stops scheduling the threads of task `tid` (the `task_stop` system call).
///
/// Each keeps its place in whatever it was blocked in: a reply or message that arrives
/// meanwhile only makes it runnable once the task is continued. One running on another CPU
/// stops when it next returns to user mode (its CPU is interrupted for that).
pub(crate) fn task_stop(tid: u64) -> i64 {
    let mut t = table();
    match t.controlled_slot(tid) {
        Ok(slot) => {
            t.tasks[slot].stopped = true;
            for i in t.threads_of(slot) {
                if t.threads[i].cpu != NO_CPU {
                    t.kick(t.threads[i].cpu);
                }
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
            t.tasks[slot].stopped = false;
            t.kick_idle();
            0
        }
        Err(e) => e,
    }
}

/// Makes a thread of task `tid` enter a handler (the `task_upcall` system call).
///
/// `request` points at the caller's [`upcall::WORDS`] words (see
/// [`haisuli_abi::upcall`]); with [`upcall::THREAD`], `tid` names the thread. The handler is
/// entered when the thread next returns to user mode; its interruptible wait ends with `-EINTR`
/// now.
pub(crate) fn task_upcall(tid: u64, request: u64) -> i64 {
    let mut bytes = [0u8; upcall::WORDS * 8];
    if !vmm::copy_from_user(current_space(), request, &mut bytes) {
        return -EFAULT;
    }
    let mut words = [0u64; upcall::WORDS];
    for (w, b) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *w = u64::from_ne_bytes(*b);
    }
    let Some(up) = Upcall::from_words(&words) else {
        return -EINVAL;
    };
    let to_thread = words[upcall::WORDS - 1] & upcall::THREAD != 0;
    let mut t = table();
    let target = if to_thread {
        t.thread_slot(tid).ok_or(-ESRCH).and_then(|th| {
            t.controlled_slot(t.tasks[t.threads[th].task].id)
                .map(|slot| (slot, th))
        })
    } else {
        t.controlled_slot(tid)
            .map(|slot| (slot, upcall_target(&t, slot)))
    };
    let (slot, th) = match target {
        Ok(target) => target,
        Err(e) => return e,
    };
    if (0..MAX_THREADS).any(|i| t.threads[i].task == slot && t.threads[i].upcall.is_some()) {
        return -EBUSY;
    }
    t.threads[th].upcall = Some(up);
    if interrupt_sleep(&mut t.threads[th]) {
        t.kick_idle();
    }
    let cpu = t.threads[th].cpu;
    if cpu != NO_CPU {
        t.kick(cpu); // It enters the handler when it next returns to user mode: soon.
    }
    0
}

/// The thread of the task in `slot` that an upcall goes to: one in an interruptible wait, else
/// one that runs or may run, else any.
fn upcall_target(t: &Table, slot: usize) -> usize {
    t.threads_of(slot)
        .find(|&i| t.threads[i].interruptible())
        .or_else(|| {
            t.threads_of(slot)
                .find(|&i| t.threads[i].state == State::Ready)
        })
        .or_else(|| t.threads_of(slot).next())
        .expect("a live task without threads")
}

/// Reports on task `tid` (the `task_info` system call): a [`TaskInfo`] in `buf`.
pub(crate) fn task_info(tid: u64, buf: u64, len: u64) -> i64 {
    if len < TASK_INFO_LEN as u64 {
        return -EINVAL;
    }
    let (info, space) = {
        let t = table();
        let Some(slot) = t.live_slot(tid) else {
            return -ESRCH;
        };
        let p = &t.tasks[slot];
        let state = if p.stopped {
            task_state::STOPPED
        } else if t
            .threads_of(slot)
            .any(|i| t.threads[i].state == State::Ready)
        {
            task_state::READY
        } else {
            task_state::BLOCKED
        };
        let info = TaskInfo {
            state,
            upcall: t.threads_of(slot).any(|i| t.threads[i].upcall.is_some()),
            threads: p.threads as u64,
            name: p.name.0,
        };
        (info, t.cur().space)
    };
    if !vmm::copy_to_user(space, buf, &info.to_bytes()) {
        return -EFAULT;
    }
    0
}

/// Buffers for reading a program and its arguments into the kernel.
struct ExecBuffers {
    block: [u8; ARG_MAX],
    image: [u8; fs::MAX_FILE],
}

/// The one set of [`ExecBuffers`]: too big for a kernel stack. Only the task manager uses
/// them (`task_exec`, `task_spawn`), so they are hardly ever contended.
static EXEC: Lock<ExecBuffers> = Lock::new(ExecBuffers {
    block: [0; ARG_MAX],
    image: [0; fs::MAX_FILE],
});

/// Reads the caller's argument block at `block[..len]` and the program it names
/// from the file system, and builds the program's address space.
///
/// Returns the address space, the registers to start with and the program's name.
fn load_program(
    block: u64,
    len: u64,
    flags: u64,
) -> Result<(AddressSpace, UserContext, Name), i64> {
    if flags & !exec::PRIVILEGED != 0 {
        return Err(-EINVAL);
    }
    if flags & exec::PRIVILEGED != 0 && !current_privileged() {
        return Err(-EPERM);
    }
    if len > ARG_MAX as u64 {
        return Err(-E2BIG);
    }
    let mut buffers = EXEC.lock();
    let ExecBuffers { block: args, image } = &mut *buffers;
    let args = &mut args[..len as usize];
    if !vmm::copy_from_user(current_space(), block, args) {
        return Err(-EFAULT);
    }
    let args = Args::parse(args).map_err(|(e, _)| e)?;
    let size = fs::read_file(args.path(), image)?;
    let (space, frame) = program::load(&image[..size], &args).map_err(|(e, _)| e)?;
    Ok((space, frame, Name::new(args.name())))
}

/// Makes the task of thread `client` run a program from the file system (the `task_exec`
/// system call).
///
/// `client` must be blocked in a call to the caller, the task manager, that it has
/// received; the call is abandoned. The program replaces the task's address
/// space, and the task's other threads end. The task keeps its id and its file descriptors;
/// it loses its privileges (unless `flags` has [`exec::PRIVILEGED`]), granted devices, IRQs,
/// services, messages and upcalls, and the calls it was serving fail. On error
/// the task is left as it was.
pub(crate) fn task_exec(client: u64, block: u64, len: u64, flags: u64) -> i64 {
    let mut cslot = match table().controlled_client(client) {
        Ok(slot) => slot,
        Err(e) => return e,
    };
    let (space, frame, name) = match load_program(block, len, flags) {
        Ok(loaded) => loaded,
        Err(e) => return e,
    };
    let (slot, old_space, old_threads) = loop {
        // The caller's context is rebuilt below, which must wait until it is off its CPU. The
        // caller may not be yet: it blocked in its call, but its CPU may not have switched away
        // (an emulated CPU can be held up for a long time). That CPU needs the scheduler's lock
        // to switch, so wait without holding it, and look again once it is taken: a thread still
        // blocked in the received call cannot have gone back onto a CPU, and one that is not
        // any more abandons the exec.
        while context::on_cpu(cslot) {
            spin_loop();
        }
        let mut t = table();
        cslot = match t.controlled_client(client) {
            Ok(slot) => slot,
            Err(e) => {
                // The task ended meanwhile.
                drop(t);
                vmm::destroy_user_space(space);
                return e;
            }
        };
        if context::on_cpu(cslot) {
            continue;
        }
        let slot = t.threads[cslot].task;
        let me = percpu::index();
        // POSIX: the other threads end. Those on a CPU notice soon, and use the old address
        // space until they are off it; all of them are held until this has seen them go.
        let old_threads = t.thread_set(slot) & !(1 << cslot);
        for i in slots(old_threads) {
            let th = &mut t.threads[i];
            th.held = true;
            th.upcall = None;
            let cpu = th.cpu;
            if cpu == NO_CPU {
                th.state = State::Exited;
                t.tasks[slot].threads -= 1;
            } else {
                th.killed = true;
                if cpu != me {
                    t.kick(cpu);
                }
            }
        }
        context::init(cslot, &frame);
        t.threads[cslot] = Thread {
            state: State::Ready,
            task: slot,
            ..Thread::EMPTY
        };
        let p = &mut t.tasks[slot];
        let old_space = p.space;
        *p = Task {
            life: Life::Alive,
            id: p.id,
            threads: p.threads,
            // Pinned until the old address space is gone: the task cannot be reaped (and its
            // slot reused) meanwhile, even if it ends.
            pins: p.pins + 1,
            stopped: p.stopped,
            space,
            name,
            privileged: flags & exec::PRIVILEGED != 0,
            fds: p.fds,
            ..Task::EMPTY
        };
        let id = p.id;
        release_server_state(&mut t, id);
        irq::release_all(id);
        t.kick_later();
        break (slot, old_space, old_threads);
    };
    // The old address space goes once nothing uses it: no old thread on a CPU, no copy (only
    // this function's own pin left).
    loop {
        {
            let t = table();
            let gone = slots(old_threads)
                .all(|i| t.threads[i].state == State::Exited && !context::on_cpu(i));
            if gone && t.tasks[slot].pins == 1 {
                break;
            }
        }
        spin_loop();
    }
    {
        let mut t = table();
        for i in slots(old_threads) {
            t.threads[i].held = false;
        }
        t.tasks[slot].pins -= 1;
    }
    vmm::destroy_user_space(old_space);
    reap();
    0
}

/// Starts a program from the file system in a new task (the `task_spawn`
/// system call); task manager only.
///
/// The task's standard input, output and error are the terminal. It is
/// privileged if `flags` has [`exec::PRIVILEGED`] (which the caller must be).
/// Returns its id.
pub(crate) fn task_spawn(block: u64, len: u64, flags: u64) -> i64 {
    let slot = {
        let mut t = table();
        if !t.is_manager() {
            return -EPERM;
        }
        let Some(slot) = t.new_task_slot() else {
            return -EAGAIN;
        };
        t.reserve(slot);
        slot
    };
    let (space, frame, name) = match load_program(block, len, flags) {
        Ok(loaded) => loaded,
        Err(e) => {
            table().unreserve(slot);
            return e;
        }
    };
    let privileged = flags & exec::PRIVILEGED != 0;
    install(
        &mut table(),
        slot,
        space,
        &frame,
        name,
        privileged,
        console_fds(),
    ) as i64
}

/// Powers the machine off (the `power_off` system call); privileged tasks only.
pub(crate) fn power_off() -> i64 {
    if !current_privileged() {
        return -EPERM;
    }
    crate::println!("[kernel] powering off");
    Board::power_off(0)
}

// ---- Upcalls ------------------------------------------------------------------
//
// An upcall waits for its thread to return to user mode: at the end of a system call, or of
// an interrupt that arrived in user mode (the tick, an IPI, an IRQ) ([`return_to_user`]). If
// the thread is asleep in an interruptible wait (`ipc_recv`, `svc_wait`, a terminal `read`),
// the wait ends with `-EINTR`, or is made again after the handler if the upcall asks for that.

/// Whether thread `th` has an upcall waiting: a blocking call must end with `-EINTR`
/// so that the handler can run.
fn interrupting(th: &Thread) -> bool {
    th.upcall.is_some()
}

/// Wakes `th` if it sleeps in a wait that an upcall interrupts (also while its task is
/// stopped); returns whether it did.
///
/// The wait notices the upcall and fails with `-EINTR` (see [`interrupting`]);
/// an interrupted IPC call is withdrawn, so its server can no longer touch the
/// client's buffer.
fn interrupt_sleep(th: &mut Thread) -> bool {
    if !th.interruptible() {
        return false;
    }
    if th.state == State::BlockedCall {
        th.call.result = -EINTR;
    }
    th.state = State::Ready;
    true
}

/// What the running thread must do before it returns to user mode.
enum Before {
    /// Its task has ended: end.
    Exit,
    /// Its task is stopped, or its quantum is over: let others run first.
    Wait,
    /// Go, entering this upcall if there is one.
    Go(Option<Upcall>),
}

/// The last step on every way back to user mode, with the user registers `f`: at the end of
/// every system call (number `nr`, whose result is in `f`) and of every interrupt that
/// arrived in user mode (`nr` is `None`).
///
/// The thread ends if its task has ended, waits while its task is stopped, gives way once it
/// has had its quantum and others wait (preemption), and enters its upcall, if it has one. A
/// thread whose stack cannot take the upcall's frame has faulted.
pub(crate) fn return_to_user(f: &mut UserContext, nr: Option<u64>) {
    // After an interrupt, only if something concerns this CPU's thread (see `kick`, `tick`).
    if nr.is_none() && !ATTENTION[percpu::index()].swap(false, Ordering::Acquire) {
        return;
    }
    let up = loop {
        let before = {
            let mut t = table();
            let me = t.me();
            let cpu = percpu::index();
            let task = &t.tasks[t.threads[me].task];
            if t.threads[me].killed || !task.alive() {
                Before::Exit
            } else if task.stopped || t.cpus[cpu].resched {
                Before::Wait
            } else {
                Before::Go(t.threads[me].upcall.take())
            }
        };
        match before {
            Before::Exit => exit_current(),
            Before::Wait => schedule(),
            Before::Go(up) => break up,
        }
    };
    let Some(up) = up else {
        return;
    };
    if let Some(nr) = nr {
        if f.returned() == -EINTR && up.restart {
            // So that returning from the handler makes the call again.
            f.restart_syscall(nr);
        }
    }
    let frame = Frame::new(&up, f);
    // `copy_to_user` only accepts mapped, writable user memory.
    match Frame::address(f.sp()) {
        Some(addr) if vmm::copy_to_user(current_space(), addr, frame.as_bytes()) => {
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
pub(crate) fn rt_sigreturn(f: &mut UserContext) -> i64 {
    let mut saved = SavedContext::empty();
    if vmm::copy_from_user(current_space(), f.sp(), saved.as_mut_bytes()) && saved.restore(f) {
        f.returned()
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
    // Take the range first: another thread of the task may map memory at the same time.
    let (space, base) = {
        let mut t = table();
        let p = t.cur_mut();
        let base = p.mmap_next;
        p.mmap_next = base + (pages + 1) * 4096; // Leave a guard page.
        (p.space, base)
    };
    if vmm::map_init(space, base, pages * 4096, &[], vmm::user_prot(prot)).is_err() {
        return -ENOMEM;
    }
    base as i64
}

/// Maps the memory-mapped registers at physical `regs..regs + len` (page aligned), of a device
/// granted to it (see [`crate::device`]), into the running, privileged task's mmap region, and
/// returns their address there.
pub(crate) fn map_device(regs: u64, len: u64) -> Result<u64, i64> {
    let pages = len.div_ceil(4096);
    // Take the range first, as `mmap` does.
    let (space, base) = {
        let mut t = table();
        let p = t.cur_mut();
        if !p.privileged {
            return Err(-EPERM);
        }
        let base = p.mmap_next;
        p.mmap_next = base + (pages + 1) * 4096; // Leave a guard page.
        (p.space, base)
    };
    vmm::map_device(space, base, regs, pages).map_err(|()| -ENOMEM)?;
    Ok(base)
}

/// Lets the running, privileged task use the I/O ports `ports`, of a device granted to it (see
/// [`crate::device`]).
///
/// Fails with `-ENODEV` on an architecture without I/O ports. Other threads of the task get the
/// new permissions the next time they are switched to.
pub(crate) fn allow_ports(ports: core::ops::Range<u64>) -> Result<(), i64> {
    let mut t = table();
    let me = t.me();
    let p = t.cur_mut();
    if !p.privileged {
        return Err(-EPERM);
    }
    if !p.io.allow(ports) {
        return Err(-ENODEV);
    }
    // Takes effect on return to user mode.
    arch::Cpu::prepare_thread(me, &p.io);
    Ok(())
}

// ---- IPC: asynchronous per-task mailbox (microkernel message passing) ----

pub(crate) fn ipc_send(dest: u64, a: u64, b: u64) -> i64 {
    let mut t = table();
    let from = t.cur().id;
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

/// Bytes of what `ipc_recv` writes: `[sender, word0, word1, task]`.
const RECV_LEN: usize = 4 * 8;

/// Waits for the next message and writes `[sender, word0, word1, task]` to `out`.
///
/// Delivers, in order of priority: IRQ notifications `[0, KMSG_IRQ, line, 0]`,
/// for the task manager the reports of ended tasks (`[0, KMSG_TASK_EXIT, tid, 0]`
/// and `[0, KMSG_TASK_FAULT, tid, 0]`), mailbox messages `[task, a, b, task]`, then the oldest
/// pending call `[client thread, op, len, client task]`.
pub(crate) fn ipc_recv(out: u64) -> i64 {
    let space = current_space();
    if !vmm::user_range_ok(space, out, RECV_LEN as u64, true) {
        return -EFAULT;
    }
    loop {
        let mut t = table();
        let Some(msg) = next_message(&mut t) else {
            let me = t.me();
            if interrupting(&t.threads[me]) {
                return -EINTR;
            }
            t.threads[me].state = State::BlockedRecv;
            drop(t);
            schedule();
            continue;
        };
        drop(t);
        let mut bytes = [0u8; RECV_LEN];
        for (chunk, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(msg) {
            *chunk = word.to_ne_bytes();
        }
        vmm::copy_to_user(space, out, &bytes);
        return 0;
    }
}

/// Take the current task's next message, if any.
fn next_message(t: &mut Table) -> Option<[u64; 4]> {
    let my = t.my_task();
    let is_manager = t.manager_slot() == Some(my);
    let me = &mut t.tasks[my];
    if me.irq_pending != 0 {
        let line = me.irq_pending.trailing_zeros() as u64;
        me.irq_pending &= !(1 << line);
        return Some([PID_KERNEL, KMSG_IRQ, line, PID_KERNEL]);
    }
    if is_manager {
        for (kind, slots) in [
            (KMSG_TASK_EXIT, &mut t.ended),
            (KMSG_TASK_FAULT, &mut t.faulted),
        ] {
            if *slots != 0 {
                let slot = slots.trailing_zeros() as u64;
                *slots &= !(1 << slot);
                return Some([PID_KERNEL, kind, slot + 1, PID_KERNEL]);
            }
        }
    }
    let me = &mut t.tasks[my];
    if me.mb_len > 0 {
        let m = me.mbox[me.mb_head];
        me.mb_head = (me.mb_head + 1) % MBOX_SIZE;
        me.mb_len -= 1;
        return Some([m.from, m.a, m.b, m.from]);
    }
    let server = me.id;
    let client = (0..MAX_THREADS)
        .filter(|&i| {
            let th = &t.threads[i];
            th.state == State::BlockedCall && th.call.server == server && !th.call.received
        })
        .min_by_key(|&i| t.threads[i].call.seq)?;
    let c = &mut t.threads[client];
    c.call.received = true;
    let task = t.tasks[c.task].id;
    Some([client as u64 + 1, c.call.op, c.call.len, task])
}

// ---- IPC: synchronous calls -------------------------------------------------

/// Sends request `op` with buffer `buf[..len]` to task `dest` and waits for the reply.
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
    let me = {
        let mut t = table();
        let me = t.me();
        if interruptible && interrupting(&t.threads[me]) {
            return -EINTR;
        }
        let Some(server) = t.live_slot(dest) else {
            return -ESRCH;
        };
        if server == t.my_task() {
            return -EDEADLK;
        }
        if !vmm::user_range_ok(t.cur().space, buf, len, false) {
            return -EFAULT;
        }
        t.call_seq += 1;
        let seq = t.call_seq;
        let th = &mut t.threads[me];
        th.call = Call {
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
        th.state = State::BlockedCall;
        t.wake_receiver_here(server); // This CPU runs it next, unless another one is faster.
        me
    };
    schedule(); // Until `ipc_reply` (or the server's death) makes us ready.
    table().threads[me].call.result
}

/// Completes a received call from thread `client` (the `ipc_reply` system call).
pub(crate) fn ipc_reply(client: u64, status: u64) -> i64 {
    let mut t = table();
    match t.received_call(client) {
        Ok(slot) => {
            t.threads[slot].call.result = status as i64;
            t.make_ready_later(slot);
            0
        }
        Err(e) => e,
    }
}

/// Copies between the caller's `local[..len]` and thread `client`'s call buffer at `offset`.
///
/// This implements the `ipc_read` and `ipc_write` system calls. It copies at
/// most the rest of the client's buffer and returns the number of bytes
/// copied, or `-EACCES` if the call's [`Access`] does not allow this
/// direction. The client's task is *pinned* meanwhile: its memory stays, even if it ends.
pub(crate) fn ipc_transfer(client: u64, offset: u64, local: u64, len: u64, to_client: bool) -> i64 {
    let (call, task, client_space, my_space) = {
        let mut t = table();
        let slot = match t.received_call(client) {
            Ok(slot) => slot,
            Err(e) => return e,
        };
        let call = t.threads[slot].call;
        if !call.access.allows(to_client) {
            return -EACCES;
        }
        if offset > call.len {
            return -EINVAL;
        }
        let task = t.threads[slot].task;
        t.tasks[task].pins += 1;
        (call, task, t.tasks[task].space, t.cur().space)
    };
    let n = len.min(call.len - offset);
    let remote = call.buf + offset;
    let ok = if to_client {
        vmm::copy_user_to_user(client_space, remote, my_space, local, n)
    } else {
        vmm::copy_user_to_user(my_space, local, client_space, remote, n)
    };
    table().tasks[task].pins -= 1;
    reap();
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
    let me = t.cur().id;
    let holder = t.services[id as usize];
    if holder != 0 && holder != me && t.live_slot(holder).is_some() {
        return -EBUSY;
    }
    t.services[id as usize] = me;
    for th in t.threads.iter_mut() {
        th.wake_from(State::BlockedService); // They retry their lookup.
    }
    t.kick_idle();
    0
}

/// The live task registered as service `id` (valid), if there is one.
fn service(t: &Table, id: u64) -> Option<u64> {
    let tid = t.services[id as usize];
    t.live_slot(tid).map(|_| tid)
}

/// Returns the task providing service `id` (the `svc_lookup` system call).
pub(crate) fn svc_lookup(id: u64) -> i64 {
    if id == 0 || id >= svc::MAX {
        return -EINVAL;
    }
    service(&table(), id).map_or(-ESRCH, |tid| tid as i64)
}

/// Returns the task providing service `id`, waiting for it to register (the
/// `svc_wait` system call).
///
/// `interruptible`: whether an upcall ends the wait with `-EINTR`.
///
/// The lookup and going to sleep happen under one hold of the lock: a registration on another
/// CPU in between would otherwise wake the waiters before this thread is one of them, and it
/// would sleep until the next registration.
fn wait_for_service(id: u64, interruptible: bool) -> i64 {
    if id == 0 || id >= svc::MAX {
        return -EINVAL;
    }
    loop {
        {
            let mut t = table();
            if let Some(tid) = service(&t, id) {
                return tid as i64;
            }
            let me = t.me();
            if interruptible && interrupting(&t.threads[me]) {
                return -EINTR;
            }
            t.threads[me].state = State::BlockedService;
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
