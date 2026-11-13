// SPDX-License-Identifier: GPL-2.0

//! Signals.
//!
//! Generation, blocking and delivery of POSIX signals, and the system calls that manage them.
//!
//! A signal is *generated* for a process ([`post`]) by `kill`, by the kernel (`SIGCHLD`) or by a
//! server on a process's behalf (the console's `SIGINT` on Ctrl-C). Generation already applies
//! what cannot wait: `SIGKILL` and `SIGCONT` resume a stopped process, a stop signal with the
//! default action stops it at once, and a signal that would be ignored is discarded. Anything
//! else becomes *pending*. If it is not blocked, the process is woken from any blocking call,
//! which fails with `EINTR`.
//!
//! Pending, unblocked signals are *delivered* on the way back to user mode
//! ([`return_to_user`]): the default action (terminate, stop, continue, ignore) or a handler.
//! For a handler, the kernel saves the user registers and the blocked signals in a frame on the
//! user stack and enters the handler, which returns to its restorer and `rt_sigreturn`. With
//! `SA_RESTART`, a call that failed with `EINTR` is restarted after the handler instead.
//!
//! Stopping needs no special state in the scheduler: a stopped process keeps its place in
//! whatever call it was blocked in and is simply not scheduled. So a stopped `pause` stays in
//! `pause`, and a `SIGTERM` for a stopped process stays pending until it continues.

use super::{exit_current, schedule, table, Process, Report, State, Table, INIT_PID};
use crate::arch::entry::TrapFrame;
use crate::vmm;
use haisuli_abi::errno::{EFAULT, EINTR, EINVAL, EPERM, ESRCH};
use haisuli_abi::nr;
use haisuli_abi::signal::*;

/// Signals whose default action stops the process.
const STOP_SIGNALS: u64 = bit(SIGSTOP) | bit(SIGTSTP) | bit(SIGTTIN) | bit(SIGTTOU);
/// Signals that can be neither caught, blocked nor ignored.
const UNBLOCKABLE: u64 = bit(SIGKILL) | bit(SIGSTOP);
/// Every valid signal.
const ALL: u64 = (1 << (NSIG - 1)) - 1;

/// Bytes below the interrupted stack pointer that a handler frame must not overwrite (the
/// SysV ABI's red zone).
const RED_ZONE: u64 = 128;
/// Size of the `siginfo_t` passed to `SA_SIGINFO` handlers.
const SIGINFO_WORDS: usize = 16;
/// Saved user registers in a frame: the general-purpose ones, rip, rflags and rsp.
const REG_WORDS: usize = 18;
/// A handler frame: the return address (the restorer), the signal, the blocked signals to
/// restore, the registers to restore, and the `siginfo_t`.
const FRAME_WORDS: usize = 3 + REG_WORDS + SIGINFO_WORDS;
const FRAME_BYTES: usize = FRAME_WORDS * 8;
const FRAME_SIGINFO: usize = 3 + REG_WORDS;
/// The rflags bits that a signal frame may change: CF, PF, AF, ZF, SF, DF and OF.
const USER_FLAGS: u64 = 0xCD5;
/// Trap flag and direction flag, cleared for a handler.
const TF_DF: u64 = 0x100 | 0x400;
/// Bytes of the `syscall` instruction, to restart an interrupted call.
const SYSCALL_LEN: u64 = 2;

/// A process's signal state.
#[derive(Clone, Copy)]
pub(super) struct Signals {
    /// Generated and not delivered yet.
    pub(super) pending: u64,
    /// Blocked: stay pending until unblocked.
    pub(super) blocked: u64,
    /// The action for each signal (index 0 is unused).
    pub(super) actions: [SigAction; NSIG as usize],
}

impl Signals {
    /// No signals pending or blocked; every action is the default.
    pub(super) const EMPTY: Signals = Signals {
        pending: 0,
        blocked: 0,
        actions: [SigAction {
            handler: SIG_DFL,
            flags: 0,
            restorer: 0,
            mask: 0,
        }; NSIG as usize],
    };
}

/// What delivering a signal does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Terminate,
    Ignore,
    Stop,
    Continue,
    Handler,
}

/// The default action for `sig`.
fn default_action(sig: u32) -> Disposition {
    match sig {
        SIGCHLD | SIGURG | SIGWINCH => Disposition::Ignore,
        SIGCONT => Disposition::Continue,
        _ if STOP_SIGNALS & bit(sig) != 0 => Disposition::Stop,
        _ => Disposition::Terminate,
    }
}

/// What delivering `sig` with action `act` does.
fn disposition(sig: u32, act: &SigAction) -> Disposition {
    match act.handler {
        SIG_DFL => default_action(sig),
        SIG_IGN => Disposition::Ignore,
        _ => Disposition::Handler,
    }
}

/// Whether delivering `sig` with action `act` would have no effect.
///
/// Such signals are discarded when they are generated (unless blocked): `SIGCONT` does its
/// work at generation.
fn ignored(sig: u32, act: &SigAction) -> bool {
    matches!(
        disposition(sig, act),
        Disposition::Ignore | Disposition::Continue
    )
}

/// Whether a parent with `SIGCHLD` action `act` has its children reaped automatically.
pub(super) fn reaps_children(act: &SigAction) -> bool {
    act.handler == SIG_IGN || act.flags & SA_NOCLDWAIT != 0
}

/// Whether a blocking call of `p` must fail with `EINTR`: a signal waits for delivery.
pub(super) fn interrupted(p: &Process) -> bool {
    p.sig.pending & !p.sig.blocked != 0
}

/// Generates `sig` (1 to 31) for the process in `slot` (see the module documentation).
pub(super) fn post(t: &mut Table, slot: usize, sig: u32) {
    let p = &mut t.procs[slot];
    if !p.alive() {
        return;
    }
    let sig_bit = bit(sig);
    let act = p.sig.actions[sig as usize];
    // Like Linux, init only gets the signals it has handlers for: the system depends on it.
    if p.pid == INIT_PID && act.handler == SIG_DFL {
        return;
    }
    if sig == SIGKILL {
        p.stopped = false;
    } else if sig == SIGCONT {
        p.sig.pending &= !STOP_SIGNALS;
        if p.stopped {
            p.stopped = false;
            p.report = Report::Continued;
            let ppid = p.ppid;
            notify_parent(t, ppid);
        }
    } else if STOP_SIGNALS & sig_bit != 0 {
        p.sig.pending &= !bit(SIGCONT);
        if act.handler == SIG_DFL && p.sig.blocked & sig_bit == 0 {
            stop(t, slot, sig);
            return;
        }
    }
    let p = &mut t.procs[slot];
    let blocked = p.sig.blocked & sig_bit != 0;
    if !blocked && ignored(sig, &act) {
        return;
    }
    p.sig.pending |= sig_bit;
    if !blocked {
        wake(p);
    }
}

/// Makes a blocked `p` ready, so that its blocking call fails with `EINTR`.
fn wake(p: &mut Process) {
    match p.state {
        State::BlockedRecv | State::BlockedWait | State::BlockedService | State::BlockedPause => {
            p.state = State::Ready;
        }
        State::BlockedCall => {
            // Cancels the call: the server can no longer reply or transfer data.
            p.call.result = -EINTR;
            p.state = State::Ready;
        }
        State::Unused | State::Ready | State::Zombie => {}
    }
}

/// Stops the process in `slot` with stop signal `sig`.
fn stop(t: &mut Table, slot: usize, sig: u32) {
    let p = &mut t.procs[slot];
    p.stopped = true;
    p.report = Report::Stopped(sig);
    let ppid = p.ppid;
    notify_parent(t, ppid);
}

/// Tells the parent `ppid` that a child stopped or continued.
///
/// It gets `SIGCHLD` unless its action for it has `SA_NOCLDSTOP`, and a wake-up for `wait4`.
fn notify_parent(t: &mut Table, ppid: u64) {
    let Some(parent) = t.live_slot(ppid) else {
        return;
    };
    if t.procs[parent].sig.actions[SIGCHLD as usize].flags & SA_NOCLDSTOP == 0 {
        post(t, parent, SIGCHLD);
    }
    if t.procs[parent].state == State::BlockedWait {
        t.procs[parent].state = State::Ready;
    }
}

/// Whether the current process may send signals to `target`.
///
/// There are no users yet: privileged processes (the boot modules) may signal anyone, and
/// ordinary processes anyone but privileged ones.
fn may_signal(sender: &Process, target: &Process) -> bool {
    sender.privileged || !target.privileged
}

/// The processes a signal is sent to.
#[derive(Clone, Copy)]
enum Target {
    Process(u64),
    /// A process group (0: the caller's).
    Group(u64),
    /// Every process but init and the caller.
    All,
}

/// Sends `sig` to a process or process group (the `kill` system call).
///
/// `pid` > 0 is a process, 0 the caller's process group, -1 every process but init and the
/// caller, and < -1 process group `-pid`.
pub(crate) fn kill(pid: i32, sig: u64) -> i64 {
    let target = match pid {
        1.. => Target::Process(pid as u64),
        -1 => Target::All,
        _ => Target::Group(pid.unsigned_abs() as u64),
    };
    send(target, sig)
}

/// Sends `sig` to process group `pgrp` (0: the caller's; the `killpg` system call).
pub(crate) fn killpg(pgrp: i32, sig: u64) -> i64 {
    if pgrp < 0 {
        return -EINVAL;
    }
    send(Target::Group(pgrp as u64), sig)
}

/// Sends `sig` (0: only check) to `target`.
///
/// Zombies count as existing, but nothing is delivered to them. Fails with `ESRCH` if no
/// process matches, and with `EPERM` if the caller may signal none of them.
fn send(target: Target, sig: u64) -> i64 {
    if sig >= NSIG as u64 {
        return -EINVAL;
    }
    let sig = sig as u32;
    let mut t = table();
    let (my_pid, my_pgid) = (t.cur().pid, t.cur().pgid);
    let matches = |p: &Process| match target {
        Target::Process(pid) => p.pid == pid,
        Target::Group(0) => p.pgid == my_pgid,
        Target::Group(pgrp) => p.pgid == pgrp,
        Target::All => p.pid != INIT_PID && p.pid != my_pid,
    };
    let mut targets = [false; super::MAX_PROCS];
    let (mut found, mut allowed) = (false, false);
    for (slot, p) in t.procs.iter().enumerate() {
        if p.exists() && matches(p) {
            found = true;
            if may_signal(t.cur(), p) {
                allowed = true;
                targets[slot] = true;
            }
        }
    }
    if sig != 0 {
        for slot in (0..super::MAX_PROCS).filter(|&s| targets[s]) {
            post(&mut t, slot, sig);
        }
    }
    match (found, allowed) {
        (false, _) => -ESRCH,
        (true, false) => -EPERM,
        (true, true) => 0,
    }
}

/// Waits for a signal handler to run (the `pause` system call); always fails with `EINTR`.
pub(crate) fn pause() -> i64 {
    loop {
        let mut t = table();
        if interrupted(t.cur()) {
            return -EINTR;
        }
        t.cur_mut().state = State::BlockedPause;
        drop(t);
        schedule();
    }
}

/// Examines and changes the action for `sig` (the `rt_sigaction` system call).
///
/// `act` and `old` point to a [`SigAction`] (or are 0). Setting an action that ignores the
/// signal discards it if pending.
pub(crate) fn sigaction(sig: u64, act: u64, old: u64, sigsetsize: u64) -> i64 {
    if sigsetsize != 8 || sig == 0 || sig >= NSIG as u64 {
        return -EINVAL;
    }
    let sig = sig as u32;
    if act != 0 && UNBLOCKABLE & bit(sig) != 0 {
        return -EINVAL;
    }
    let pml4 = super::current_pml4();
    let new = if act == 0 {
        None
    } else {
        let mut w = [0u8; 32];
        if !vmm::copy_from_user(pml4, act, &mut w) {
            return -EFAULT;
        }
        let word = |i: usize| u64::from_ne_bytes(w[i * 8..i * 8 + 8].try_into().unwrap());
        Some(SigAction {
            handler: word(0),
            flags: word(1),
            restorer: word(2),
            mask: word(3) & ALL & !UNBLOCKABLE,
        })
    };
    let mut t = table();
    let p = t.cur_mut();
    let previous = p.sig.actions[sig as usize];
    if old != 0 {
        let mut w = [0u8; 32];
        for (i, v) in [
            previous.handler,
            previous.flags,
            previous.restorer,
            previous.mask,
        ]
        .iter()
        .enumerate()
        {
            w[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
        }
        if !vmm::copy_to_user(pml4, old, &w) {
            return -EFAULT;
        }
    }
    if let Some(new) = new {
        p.sig.actions[sig as usize] = new;
        if ignored(sig, &new) {
            p.sig.pending &= !bit(sig);
        }
    }
    0
}

/// Examines and changes the blocked signals (the `rt_sigprocmask` system call).
///
/// `SIGKILL` and `SIGSTOP` cannot be blocked; trying is not an error.
pub(crate) fn sigprocmask(how: u64, set: u64, old: u64, sigsetsize: u64) -> i64 {
    if sigsetsize != 8 {
        return -EINVAL;
    }
    let pml4 = super::current_pml4();
    let mut t = table();
    let p = t.cur_mut();
    let previous = p.sig.blocked;
    if set != 0 {
        let mut w = [0u8; 8];
        if !vmm::copy_from_user(pml4, set, &mut w) {
            return -EFAULT;
        }
        let set = u64::from_ne_bytes(w);
        let blocked = match how {
            SIG_BLOCK => previous | set,
            SIG_UNBLOCK => previous & !set,
            SIG_SETMASK => set,
            _ => return -EINVAL,
        };
        p.sig.blocked = blocked & ALL & !UNBLOCKABLE;
    }
    if old != 0 && !vmm::copy_to_user(pml4, old, &previous.to_ne_bytes()) {
        p.sig.blocked = previous;
        return -EFAULT;
    }
    0
}

/// Delivers pending signals before the current process returns to user mode.
///
/// Called at the end of every system call with the user registers and the call's number (its
/// result is already in `f.rax`). Waits while the process is stopped, then delivers signals
/// until one needs a handler, which it sets up, or none are left. Does not return if a signal
/// terminates the process.
pub(crate) fn return_to_user(f: &mut TrapFrame, nr: u64) {
    loop {
        let next = {
            let mut t = table();
            let current = t.current;
            let p = t.cur_mut();
            if p.stopped {
                None
            } else {
                let deliverable = p.sig.pending & !p.sig.blocked;
                if deliverable == 0 {
                    return;
                }
                let sig = deliverable.trailing_zeros() + 1;
                p.sig.pending &= !bit(sig);
                let act = p.sig.actions[sig as usize];
                match disposition(sig, &act) {
                    Disposition::Ignore | Disposition::Continue => continue,
                    Disposition::Stop => {
                        stop(&mut t, current, sig);
                        continue;
                    }
                    Disposition::Terminate | Disposition::Handler => Some((sig, act)),
                }
            }
        };
        match next {
            None => schedule(), // Stopped: runs again after SIGCONT or SIGKILL.
            Some((sig, act)) if act.handler == SIG_DFL => exit_current(sig as i32),
            Some((sig, act)) => {
                enter_handler(f, nr, sig, &act);
                return;
            }
        }
    }
}

/// Saves the user state in a frame on the user stack and makes `f` enter the handler.
///
/// Terminates the process with `SIGSEGV` if the frame does not fit on its stack.
fn enter_handler(f: &mut TrapFrame, nr: u64, sig: u32, act: &SigAction) {
    let mut saved = *f;
    // A call that the signal interrupted starts again after the handler, if the action asks for
    // it. `pause` never restarts: it exists to wait for a handler.
    if f.rax as i64 == -EINTR
        && act.flags & SA_RESTART != 0
        && nr != nr::PAUSE
        && nr != nr::RT_SIGRETURN
    {
        saved.rip -= SYSCALL_LEN;
        saved.rax = nr;
    }
    let (pml4, blocked) = {
        let t = table();
        (t.cur().pml4, t.cur().sig.blocked)
    };
    // The handler starts as if called: rsp + 8 is 16-byte aligned.
    let frame = saved
        .rsp
        .checked_sub(RED_ZONE + FRAME_BYTES as u64)
        .and_then(|a| (a & !15).checked_sub(8));
    let mut words = [0u64; FRAME_WORDS];
    words[0] = act.restorer;
    words[1] = sig as u64;
    words[2] = blocked;
    words[3..3 + REG_WORDS].copy_from_slice(&[
        saved.r15,
        saved.r14,
        saved.r13,
        saved.r12,
        saved.r11,
        saved.r10,
        saved.r9,
        saved.r8,
        saved.rbp,
        saved.rdi,
        saved.rsi,
        saved.rdx,
        saved.rcx,
        saved.rbx,
        saved.rax,
        saved.rip,
        saved.rflags,
        saved.rsp,
    ]);
    words[FRAME_SIGINFO] = sig as u64; // `si_signo`; `si_errno` and `si_code` (SI_USER) are 0.
    let mut bytes = [0u8; FRAME_BYTES];
    for (i, w) in words.iter().enumerate() {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_ne_bytes());
    }
    let Some(frame) = frame.filter(|&a| vmm::copy_to_user(pml4, a, &bytes)) else {
        exit_current(SIGSEGV as i32);
    };
    f.rip = act.handler;
    f.rsp = frame;
    f.rdi = sig as u64;
    f.rsi = if act.flags & SA_SIGINFO != 0 {
        frame + FRAME_SIGINFO as u64 * 8
    } else {
        0
    };
    f.rdx = 0;
    f.rax = 0;
    f.rflags &= !TF_DF;
    let mut t = table();
    let p = t.cur_mut();
    if act.flags & SA_NODEFER == 0 {
        p.sig.blocked |= bit(sig);
    }
    p.sig.blocked = (p.sig.blocked | act.mask) & !UNBLOCKABLE;
    if act.flags & SA_RESETHAND != 0 {
        p.sig.actions[sig as usize] = Signals::EMPTY.actions[0];
    }
}

/// Returns from a signal handler (the `rt_sigreturn` system call).
///
/// Restores the registers and blocked signals saved by [`enter_handler`], in the frame just
/// above the stack pointer (the handler has returned to the restorer). The frame is user
/// memory, so only user-mode state is taken from it: rflags only the arithmetic and direction
/// flags, and rip and rsp only user addresses. A bad frame terminates the process with
/// `SIGSEGV`. Returns the restored rax.
pub(crate) fn sigreturn(f: &mut TrapFrame) -> i64 {
    let pml4 = super::current_pml4();
    let mut bytes = [0u8; FRAME_BYTES];
    let frame = f.rsp.wrapping_sub(8);
    if !vmm::copy_from_user(pml4, frame, &mut bytes) {
        exit_current(SIGSEGV as i32);
    }
    let word = |i: usize| u64::from_ne_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
    let r = |i: usize| word(3 + i);
    let (rip, rflags, rsp) = (r(15), r(16), r(17));
    if rip >= vmm::USER_LIMIT || rsp >= vmm::USER_LIMIT {
        exit_current(SIGSEGV as i32);
    }
    f.r15 = r(0);
    f.r14 = r(1);
    f.r13 = r(2);
    f.r12 = r(3);
    f.r11 = r(4);
    f.r10 = r(5);
    f.r9 = r(6);
    f.r8 = r(7);
    f.rbp = r(8);
    f.rdi = r(9);
    f.rsi = r(10);
    f.rdx = r(11);
    f.rcx = r(12);
    f.rbx = r(13);
    f.rip = rip;
    f.rflags = (f.rflags & !USER_FLAGS) | (rflags & USER_FLAGS);
    f.rsp = rsp;
    table().cur_mut().sig.blocked = word(2) & ALL & !UNBLOCKABLE;
    r(14) as i64
}
