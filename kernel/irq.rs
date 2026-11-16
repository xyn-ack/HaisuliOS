// SPDX-License-Identifier: GPL-2.0

//! Hardware interrupts.
//!
//! Forwards the platform's interrupt lines to user-space drivers as IPC notifications, and
//! dispatches every interrupt to where it belongs.
//!
//! The kernel contains no device drivers. A privileged task binds an IRQ line (`irq_bind`); when
//! the line fires, the platform's interrupt controller ([`InterruptController`]) masks it and
//! the kernel posts a `[PID_KERNEL, KMSG_IRQ, line, 0]` notification to the owner. The driver
//! services its device and re-enables the line with `irq_ack`. Lines without an owner stay
//! masked.
//!
//! The CPU's own interrupts, the scheduler's tick and the reschedule IPI ([`CpuInterrupts`]),
//! arrive here too ([`dispatch`]); on the way back to user mode, any interrupt may preempt the
//! thread or make it enter an upcall, exactly as the end of a system call does. The
//! architecture's interrupt entry says where an interrupt came from ([`Interrupt`]); this module
//! knows nothing of vectors or interrupt ids.
//!
//! Interrupts are only ever enabled in user mode and while a CPU idles
//! ([`crate::task::idle`]), never elsewhere in the kernel.

use crate::arch::{Cpu, CpuInterrupts, Interrupt, UserContext, UserRegs};
use crate::platform::{Board, InterruptController};
use crate::{clock, task};
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::errno::{EBUSY, EINVAL, EPERM};

/// Interrupt lines the platform has.
pub(crate) const LINES: usize = Board::LINES;

/// The task owning each line (0 = unbound).
static OWNER: [AtomicU64; LINES] = [const { AtomicU64::new(0) }; LINES];

/// Sets the platform's interrupt controller up, with every device line masked.
pub(crate) fn init() {
    Board::init();
}

/// Sets up the interrupt controller's part for the CPU this runs on (every CPU).
pub(crate) fn init_cpu() {
    Board::init_cpu();
}

/// Handles an interrupt from `source`, with interrupts disabled; `f` holds the interrupted
/// registers. Called by the architecture's interrupt entry.
pub(crate) fn dispatch(source: Interrupt, f: &mut UserContext) {
    match source {
        Interrupt::Line(line) => line_interrupt(line),
        Interrupt::Tick => {
            Cpu::end_of_interrupt();
            clock::tick();
            task::tick();
        }
        Interrupt::Reschedule => Cpu::end_of_interrupt(),
        Interrupt::Spurious => {}
    }
    if f.in_user_mode() {
        task::return_to_user(f, None);
    }
}

/// An interrupt line fired: tell its owner.
fn line_interrupt(line: usize) {
    if line >= LINES || !Board::acknowledge(line) {
        return; // Spurious.
    }
    let owner = OWNER[line].load(Ordering::Relaxed);
    if owner != 0 {
        task::irq_notify(owner, line as u64);
    }
}

/// Returns the task owning `line`, or 0.
pub(crate) fn owner(line: usize) -> u64 {
    OWNER[line].load(Ordering::Relaxed)
}

/// Routes IRQ `line` to the calling, privileged task (the `irq_bind` system call).
pub(crate) fn bind(line: u64) -> i64 {
    if line >= LINES as u64 || Board::is_reserved(line as usize) {
        return -EINVAL;
    }
    if !task::current_privileged() {
        return -EPERM;
    }
    let me = task::current_tid();
    let owner = OWNER[line as usize].load(Ordering::Relaxed);
    if owner != 0 && owner != me && task::is_alive(owner) {
        return -EBUSY;
    }
    OWNER[line as usize].store(me, Ordering::Relaxed);
    Board::unmask(line as usize);
    0
}

/// Unmasks `line` once its owner has serviced the device (the `irq_ack` system call).
pub(crate) fn ack(line: u64) -> i64 {
    if line >= LINES as u64 {
        return -EINVAL;
    }
    if owner(line as usize) != task::current_tid() {
        return -EPERM;
    }
    Board::unmask(line as usize);
    0
}

/// Unbind and mask every line owned by task `tid` (it has ended).
pub(crate) fn release_all(tid: u64) {
    for (line, owner) in OWNER.iter().enumerate() {
        if owner.load(Ordering::Relaxed) == tid {
            owner.store(0, Ordering::Relaxed);
            Board::mask(line);
        }
    }
}
