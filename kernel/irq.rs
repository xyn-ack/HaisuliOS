// SPDX-License-Identifier: GPL-2.0

//! Hardware interrupts.
//!
//! Forwards the platform's interrupt lines to user-space drivers as IPC notifications, and
//! dispatches every interrupt to where it belongs.
//!
//! The kernel contains no device drivers. A privileged task binds an IRQ line (`irq_bind`); when
//! the line fires, the platform's interrupt controller ([`crate::platform::pic`]) masks it and
//! the kernel posts a `[PID_KERNEL, KMSG_IRQ, line, 0]` notification to the owner. The driver
//! services its device and re-enables the line with `irq_ack`. Lines without an owner stay
//! masked.
//!
//! The CPU's own interrupts, the scheduler's tick and the reschedule IPI
//! ([`crate::arch::apic`]), arrive here too ([`interrupt_dispatch`]); on the way back to user
//! mode, any interrupt may preempt the thread or make it enter an upcall, exactly as the end of
//! a system call does.
//!
//! Interrupts are only ever enabled in user mode and while a CPU idles
//! ([`crate::task::idle`]), never elsewhere in the kernel.

use crate::arch::apic::{self, RESCHEDULE_VECTOR, SPURIOUS_VECTOR, TIMER_VECTOR};
use crate::arch::idt::IRQ_VECTOR_BASE;
use crate::arch::{UserContext, UserRegs};
use crate::platform::pic;
use crate::task;
use core::sync::atomic::{AtomicU64, Ordering};
use haisuli_abi::errno::{EBUSY, EINVAL, EPERM};

/// Interrupt lines the platform has.
pub(crate) const LINES: usize = pic::LINES;

/// The task owning each line (0 = unbound).
static OWNER: [AtomicU64; LINES] = [const { AtomicU64::new(0) }; LINES];

/// Sets the platform's interrupt controller up, with every device line masked.
pub(crate) fn init() {
    pic::init();
}

/// Called (with interrupts disabled) by the interrupt stubs in `arch::entry`, with the vector
/// and the interrupted registers.
pub(crate) extern "C" fn interrupt_dispatch(vector: u64, f: &mut UserContext) {
    let pic_vectors = IRQ_VECTOR_BASE as u64..(IRQ_VECTOR_BASE + LINES) as u64;
    match vector {
        v if pic_vectors.contains(&v) => pic_interrupt(v - IRQ_VECTOR_BASE as u64),
        v if v == TIMER_VECTOR as u64 => {
            apic::end_of_interrupt();
            task::tick();
        }
        v if v == RESCHEDULE_VECTOR as u64 => apic::end_of_interrupt(),
        v if v == SPURIOUS_VECTOR as u64 => {} // Needs no end of interrupt.
        _ => {}
    }
    if f.in_user_mode() {
        task::return_to_user(f, None);
    }
}

/// An interrupt line fired: tell its owner.
fn pic_interrupt(line: u64) {
    if !pic::acknowledge(line) {
        return; // Spurious.
    }
    let owner = OWNER[line as usize].load(Ordering::Relaxed);
    if owner != 0 {
        task::irq_notify(owner, line);
    }
}

/// Returns the task owning `line`, or 0.
pub(crate) fn owner(line: usize) -> u64 {
    OWNER[line].load(Ordering::Relaxed)
}

/// Routes IRQ `line` to the calling, privileged task (the `irq_bind` system call).
pub(crate) fn bind(line: u64) -> i64 {
    if line >= LINES as u64 || line == pic::RESERVED_LINE {
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
    pic::unmask(line);
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
    pic::unmask(line);
    0
}

/// Unbind and mask every line owned by task `tid` (it has ended).
pub(crate) fn release_all(tid: u64) {
    for (line, owner) in OWNER.iter().enumerate() {
        if owner.load(Ordering::Relaxed) == tid {
            owner.store(0, Ordering::Relaxed);
            pic::mask(line as u64);
        }
    }
}
