// SPDX-License-Identifier: GPL-2.0

//! Page tables.
//!
//! arm64's stage 1 translation tables with a 4 KiB granule, as [`crate::arch::PageTables`]
//! describes them to the generic [`crate::vmm`]: four levels of 512 entries of 8 bytes, each
//! level translating 9 bits above a 12-bit page offset, 48-bit virtual addresses.
//!
//! User space has `TTBR0_EL1` to itself: every address space is one root table there, which
//! starts empty (so [`PageTables::init_root`] has nothing to do). The kernel lives in
//! `TTBR1_EL1`, which the boot code points at the boot tables for good: the kernel image and the
//! linear map of physical memory (see [`super::boot`]). A CPU that runs no thread has the empty
//! root in `TTBR0_EL1`.

use super::{cpu, KERNEL_OFFSET};
use crate::arch::{PageTables, Prot};
use core::ops::Range;
use core::sync::atomic::AtomicU64;

/// Descriptor bits (Arm ARM D8.3).
pub(crate) mod desc {
    /// The entry is valid.
    pub(crate) const VALID: u64 = 1 << 0;
    /// Above the last level: a table (otherwise a block); at the last level: a page.
    pub(crate) const TABLE_OR_PAGE: u64 = 1 << 1;
    /// AttrIndx: which MAIR_EL1 attribute applies (see [`MAIR`](super::mair::MAIR)).
    pub(crate) const fn attr(index: u64) -> u64 {
        index << 2
    }
    /// AP\[1\]: EL0 may access it.
    pub(crate) const USER: u64 = 1 << 6;
    /// AP\[2\]: read-only.
    pub(crate) const READ_ONLY: u64 = 1 << 7;
    /// SH: inner shareable (all the CPUs).
    pub(crate) const INNER_SHAREABLE: u64 = 0b11 << 8;
    /// AF: accessed (otherwise the first access faults).
    pub(crate) const ACCESSED: u64 = 1 << 10;
    /// nG: not global, tied to the address space (every user mapping).
    pub(crate) const NOT_GLOBAL: u64 = 1 << 11;
    /// PXN: EL1 may not execute it.
    pub(crate) const PRIVILEGED_NO_EXECUTE: u64 = 1 << 53;
    /// UXN: EL0 may not execute it.
    pub(crate) const USER_NO_EXECUTE: u64 = 1 << 54;
    /// A bit for software (ignored by the CPU), which HaisuliOS sets in the entries that map
    /// device registers rather than memory.
    pub(crate) const DEVICE: u64 = 1 << 55;
    /// The bits of an entry that hold the physical address it points to.
    pub(crate) const ADDRESS: u64 = 0x0000_FFFF_FFFF_F000;
}

/// The memory attribute indexes, and MAIR_EL1 with those attributes.
pub(crate) mod mair {
    /// Normal memory, write-back cacheable.
    pub(crate) const NORMAL: u64 = 0;
    /// Device memory (nGnRnE): registers, never cached, gathered, reordered or speculated.
    pub(crate) const DEVICE: u64 = 1;
    /// MAIR_EL1: attribute 0 = 0xFF (normal, inner and outer write-back, read/write-allocate),
    /// attribute 1 = 0x00 (Device-nGnRnE).
    pub(crate) const MAIR: u64 = 0xFF;
}

/// A block of normal memory for the kernel (EL1 only, never executed by EL0), for the boot
/// tables' 1 GiB blocks.
pub(crate) const KERNEL_NORMAL_BLOCK: u64 = desc::VALID
    | desc::attr(mair::NORMAL)
    | desc::INNER_SHAREABLE
    | desc::ACCESSED
    | desc::USER_NO_EXECUTE;
/// A block of device registers for the kernel (EL1 only, never executed).
pub(crate) const KERNEL_DEVICE_BLOCK: u64 = desc::VALID
    | desc::attr(mair::DEVICE)
    | desc::ACCESSED
    | desc::PRIVILEGED_NO_EXECUTE
    | desc::USER_NO_EXECUTE;

/// Entries per table: 2^[`PageTables::INDEX_BITS`].
const TABLE_ENTRIES: usize = 512;

#[repr(C, align(4096))]
struct Table([AtomicU64; TABLE_ENTRIES]);

/// The root of the address space with no user pages: nothing is ever written to it.
static EMPTY_ROOT: Table = Table([const { AtomicU64::new(0) }; TABLE_ENTRIES]);

/// The physical address of the empty root.
pub(crate) fn empty_root() -> u64 {
    &EMPTY_ROOT as *const Table as u64 - KERNEL_OFFSET
}

/// Leaves the boot code's identity map behind: `TTBR0_EL1` holds the empty root from now on,
/// until the CPU runs a thread.
pub(crate) fn drop_identity_map() {
    cpu::set_user_root(empty_root());
}

/// The arm64 page-table format.
pub(crate) struct Paging;

impl PageTables for Paging {
    const LEVELS: usize = 4;
    const INDEX_BITS: u32 = 9;
    const PAGE_SHIFT: u32 = 12;
    // The whole root of `TTBR0_EL1` is user space: the lower half of the 48-bit addresses.
    const USER_ROOT_ENTRIES: Range<usize> = 0..TABLE_ENTRIES / 2;

    fn is_present(entry: u64) -> bool {
        entry & desc::VALID != 0
    }

    fn target(entry: u64) -> u64 {
        entry & desc::ADDRESS
    }

    fn table_entry(table: u64) -> u64 {
        // Tables allow everything; the page entry restricts.
        (table & desc::ADDRESS) | desc::VALID | desc::TABLE_OR_PAGE
    }

    fn page_entry(frame: u64, prot: Prot) -> u64 {
        let mut entry = (frame & desc::ADDRESS)
            | desc::VALID
            | desc::TABLE_OR_PAGE
            | desc::attr(mair::NORMAL)
            | desc::INNER_SHAREABLE
            | desc::ACCESSED
            | desc::NOT_GLOBAL
            // The kernel never runs code from user pages.
            | desc::PRIVILEGED_NO_EXECUTE;
        if prot.user {
            entry |= desc::USER;
        }
        if !prot.write {
            entry |= desc::READ_ONLY;
        }
        if !prot.exec {
            entry |= desc::USER_NO_EXECUTE;
        }
        entry
    }

    fn device_entry(frame: u64, prot: Prot) -> u64 {
        let mut entry = (frame & desc::ADDRESS)
            | desc::VALID
            | desc::TABLE_OR_PAGE
            | desc::attr(mair::DEVICE)
            | desc::ACCESSED
            | desc::NOT_GLOBAL
            | desc::PRIVILEGED_NO_EXECUTE
            | desc::USER_NO_EXECUTE
            | desc::DEVICE;
        if prot.user {
            entry |= desc::USER;
        }
        if !prot.write {
            entry |= desc::READ_ONLY;
        }
        entry
    }

    fn is_device(entry: u64) -> bool {
        entry & desc::DEVICE != 0
    }

    fn page_prot(entry: u64) -> Prot {
        Prot {
            user: entry & desc::USER != 0,
            write: entry & desc::READ_ONLY == 0,
            exec: entry & desc::USER_NO_EXECUTE == 0,
        }
    }

    fn kernel_root() -> u64 {
        empty_root()
    }

    fn init_root(_root: u64) {
        // The kernel is in TTBR1_EL1: a new root has nothing to share.
    }

    fn activate(root: u64) {
        if cpu::user_root() != root {
            cpu::set_user_root(root);
        }
    }
}
