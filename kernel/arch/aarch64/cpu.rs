// SPDX-License-Identifier: GPL-2.0

//! CPU instructions.
//!
//! Privileged arm64 instructions and system registers, each behind a safe interface whose
//! restrictions make it safe.

use core::arch::asm;

/// Stops this CPU for good.
pub(crate) fn halt() -> ! {
    loop {
        // SAFETY: Masking every exception and waiting touches no memory.
        unsafe { asm!("msr daifset, #0xf", "wfi", options(nomem, nostack)) };
    }
}

/// Sleeps until an interrupt has been handled, with interrupts masked before and after.
///
/// `wfi` with interrupts masked still wakes up when one is pending; unmasking then takes it (the
/// `isb` makes sure that it is taken here). So one that arrives before the `wfi` is not missed.
pub(crate) fn wait_for_interrupt() {
    // SAFETY: The interrupt handlers save and restore every register (the general-purpose and
    // the FP/SIMD ones). No `nomem`: handlers change kernel state, so the compiler must not cache
    // memory across this point.
    unsafe {
        asm!(
            "wfi",
            "msr daifclr, #2",
            "isb",
            "msr daifset, #2",
            options(nostack)
        )
    };
}

/// This CPU's affinity: MPIDR_EL1's Aff3, Aff2, Aff1 and Aff0 fields, as the device tree's
/// `/cpus` nodes give them.
pub(crate) fn mpidr() -> u64 {
    let v: u64;
    // SAFETY: Reading MPIDR_EL1 has no side effects.
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v & MPIDR_AFFINITY
}

/// The affinity fields of MPIDR_EL1: Aff3 (bits 32..40) and Aff2, Aff1, Aff0 (bits 0..24).
pub(crate) const MPIDR_AFFINITY: u64 = 0xFF_00FF_FFFF;

/// The exception syndrome (ESR_EL1) and fault address (FAR_EL1) of the exception being handled.
pub(crate) fn fault_syndrome() -> (u64, u64) {
    let (esr, far): (u64, u64);
    // SAFETY: Reading ESR_EL1 and FAR_EL1 has no side effects.
    unsafe {
        asm!(
            "mrs {}, esr_el1",
            "mrs {}, far_el1",
            out(reg) esr,
            out(reg) far,
            options(nomem, nostack, preserves_flags),
        )
    };
    (esr, far)
}

/// The root of the user half of the address space (TTBR0_EL1).
pub(crate) fn user_root() -> u64 {
    let v: u64;
    // SAFETY: Reading TTBR0_EL1 has no side effects.
    unsafe { asm!("mrs {}, ttbr0_el1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Makes `root` the root of the user half of the address space (TTBR0_EL1), and forgets the
/// translations of the old one.
///
/// # Panics
///
/// Panics unless [`super::phys::is_page_table_root`] accepts `root`.
pub(crate) fn set_user_root(root: u64) {
    assert!(
        super::phys::is_page_table_root(root),
        "not a page table: {:#x}",
        root
    );
    // SAFETY: `root` is the kernel's empty root or an allocator frame that `vmm` prepared; they
    // map user space only, and the kernel (in TTBR1_EL1) is unaffected. The `dsb` makes the
    // tables' last changes visible to the walker first; every translation of the old root goes.
    unsafe {
        asm!(
            "dsb ishst",
            "msr ttbr0_el1, {}",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            in(reg) root,
            options(nostack, preserves_flags),
        )
    };
}
