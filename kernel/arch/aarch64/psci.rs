// SPDX-License-Identifier: GPL-2.0

//! PSCI, the firmware interface for CPU power.
//!
//! Starting a CPU (`CPU_ON`) and powering the machine off (`SYSTEM_OFF`). The firmware is reached
//! with `hvc` or `smc`, whichever the machine says (its device tree's `/psci` node): the
//! platform tells with [`set_conduit`] at boot.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

/// `CPU_ON`, 64-bit: `(target MPIDR, entry point, context id)`.
const CPU_ON: u64 = 0xC400_0003;
/// `SYSTEM_OFF`.
const SYSTEM_OFF: u64 = 0x8400_0008;

/// Whether the firmware is called with `smc` (otherwise `hvc`).
static SMC: AtomicBool = AtomicBool::new(false);

/// How to call the firmware.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conduit {
    /// `hvc` (QEMU's `virt` without EL2 or EL3: QEMU is the firmware).
    Hvc,
    /// `smc`.
    Smc,
}

/// Sets how to call the firmware (before any call).
pub(crate) fn set_conduit(conduit: Conduit) {
    SMC.store(conduit == Conduit::Smc, Ordering::Relaxed);
}

/// Calls PSCI function `function` with three arguments; returns its result (negative: an error).
fn call(function: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    // SAFETY: PSCI calls follow the SMC calling convention: x0 to x3 carry the function and its
    // arguments, x0 the result, and x4 to x17 may be changed. The functions used here touch none
    // of the kernel's memory.
    unsafe {
        if SMC.load(Ordering::Relaxed) {
            asm!("smc #0", inlateout("x0") function as i64 => ret, inlateout("x1") a1 => _,
                 inlateout("x2") a2 => _, inlateout("x3") a3 => _, clobber_abi("C"),
                 options(nostack));
        } else {
            asm!("hvc #0", inlateout("x0") function as i64 => ret, inlateout("x1") a1 => _,
                 inlateout("x2") a2 => _, inlateout("x3") a3 => _, clobber_abi("C"),
                 options(nostack));
        }
    }
    ret
}

/// Starts the CPU with affinity `mpidr` at physical address `entry`, with the MMU off and
/// `context` in `x0`; whether the firmware accepted.
pub(crate) fn cpu_on(mpidr: u64, entry: u64, context: u64) -> bool {
    call(CPU_ON, mpidr, entry, context) == 0
}

/// Powers the machine off; returns only if the firmware cannot.
pub(crate) fn system_off() {
    call(SYSTEM_OFF, 0, 0, 0);
}
