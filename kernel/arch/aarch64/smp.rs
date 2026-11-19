// SPDX-License-Identifier: MIT

//! Starting the other CPUs.
//!
//! The boot CPU starts every other CPU that the platform lists (by MPIDR affinity: the device
//! tree's `/cpus`) with PSCI `CPU_ON`, at [`super::boot::secondary_entry`] with its index in
//! `x0`: it turns on the MMU, switches to its stack (`STACKS[index]`) and calls [`ap_start`],
//! which calls [`crate::ap_main`].
//!
//! The CPUs start one at a time, and the boot CPU waits until each one has reached Rust code
//! before it starts the next. A CPU that takes too long is given up: it and the boot CPU race
//! for one atomic [`Start`] state, and if it comes late after all, it halts at once. Then no more
//! CPUs are started.

use super::percpu::MAX_CPUS;
use super::{boot, cpu, psci};
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// Bytes of each CPU's boot stack, on which it later idles.
pub(crate) const STACK_SIZE: usize = 16 * 1024;

/// How long to wait for a CPU to reach Rust code (an emulated one on a busy host can be slow).
const START_TIMEOUT_US: u32 = 2_000_000;

/// The boot stacks of the other CPUs, by index (the boot CPU has the one from `boot`).
#[repr(C, align(16))]
pub(crate) struct Stack([AtomicU64; STACK_SIZE / 8]);
pub(crate) static STACKS: [Stack; MAX_CPUS] =
    [const { Stack([const { AtomicU64::new(0) }; STACK_SIZE / 8]) }; MAX_CPUS];

/// Where the CPU being started is (one of [`Start`]).
static STARTED: AtomicU8 = AtomicU8::new(Start::Waiting as u8);

/// The progress of the CPU being started.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Start {
    /// Not in Rust code yet.
    Waiting,
    /// In Rust code: it is the next CPU.
    Arrived,
    /// Given up by the boot CPU: if it arrives after all, it halts.
    Abandoned,
}

/// Moves the CPU being started from `Waiting` to `to`; whoever does so first decides.
fn settle(to: Start) -> bool {
    STARTED
        .compare_exchange(
            Start::Waiting as u8,
            to as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

fn arrived() -> bool {
    STARTED.load(Ordering::Acquire) == Start::Arrived as u8
}

/// The first Rust code of a CPU that PSCI started, on `STACKS[cpu]`.
pub(crate) extern "C" fn ap_start(cpu: u64) -> ! {
    if !settle(Start::Arrived) {
        cpu::halt(); // Too late: the boot CPU went on without it.
    }
    super::percpu::init(cpu as usize); // Before anything that takes a lock.
    crate::ap_main(cpu as usize)
}

/// Starts the CPUs with MPIDR affinities `ids` (the boot CPU's among them is skipped), with
/// `wait(us)` to measure how long they take.
///
/// Each started CPU gets the next index, from 1. Returns how many CPUs run, the boot CPU
/// included.
pub(crate) fn start_cpus(ids: &[u64], wait: fn(u32)) -> usize {
    let me = cpu::mpidr();
    let mut running = 1;
    for &id in ids.iter().filter(|&&id| id != me) {
        if running == MAX_CPUS {
            break;
        }
        STARTED.store(Start::Waiting as u8, Ordering::Release);
        if !psci::cpu_on(id, boot::secondary_entry_phys(), running as u64) {
            crate::println!("[kernel] CPU with MPIDR {:#x} cannot be started", id);
            continue;
        }
        let mut waited = 0;
        while !arrived() && waited < START_TIMEOUT_US {
            wait(1000);
            waited += 1000;
        }
        if arrived() || !settle(Start::Abandoned) {
            running += 1;
        } else {
            crate::println!("[kernel] CPU with MPIDR {:#x} did not start", id);
            break; // It may still come: then no other CPU may have its index.
        }
    }
    running
}
