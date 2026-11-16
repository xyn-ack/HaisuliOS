// SPDX-License-Identifier: GPL-2.0

//! Page tables.
//!
//! x86_64's 4-level page tables, as [`crate::arch::PageTables`] describes them to the generic
//! [`crate::vmm`]: the PML4, the PDPT, the page directory and the page table, 512 entries of 8
//! bytes each, translating 9 bits per level above a 12-bit page offset.
//!
//! The kernel half of every address space is the boot page tables' PML4 entry 0, the identity
//! map of the first 1 GiB (and the local APIC's page): every new root gets a copy of it, and user
//! space is the rest of the lower half, entries 1 to 255.

use super::x86::PageFlag;
use super::{boot, cpu, phys};
use crate::arch::{PageTables, Prot};
use core::ops::Range;

/// The bits of an entry that hold the physical address it points to.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Entries per table: 2^[`PageTables::INDEX_BITS`].
const TABLE_ENTRIES: usize = 512;

/// The x86_64 page-table format.
pub(crate) struct Paging;

impl PageTables for Paging {
    const LEVELS: usize = 4;
    const INDEX_BITS: u32 = 9;
    const PAGE_SHIFT: u32 = 12;
    // Entry 0 is the kernel's; the upper half is unused.
    const USER_ROOT_ENTRIES: Range<usize> = 1..TABLE_ENTRIES / 2;

    fn is_present(entry: u64) -> bool {
        entry & PageFlag::Present as u64 != 0
    }

    fn target(entry: u64) -> u64 {
        entry & ADDR_MASK
    }

    fn table_entry(table: u64) -> u64 {
        // Intermediate tables allow everything; the page entry restricts.
        (table & ADDR_MASK)
            | PageFlag::bits(&[PageFlag::Present, PageFlag::Writable, PageFlag::User])
    }

    fn page_entry(frame: u64, prot: Prot) -> u64 {
        let mut entry = (frame & ADDR_MASK) | PageFlag::Present as u64;
        if prot.user {
            entry |= PageFlag::User as u64;
        }
        if prot.write {
            entry |= PageFlag::Writable as u64;
        }
        if !prot.exec {
            entry |= PageFlag::NoExecute as u64;
        }
        entry
    }

    fn device_entry(frame: u64, prot: Prot) -> u64 {
        Self::page_entry(frame, prot)
            | PageFlag::bits(&[
                PageFlag::WriteThrough,
                PageFlag::CacheDisable,
                PageFlag::Device,
            ])
    }

    fn is_device(entry: u64) -> bool {
        entry & PageFlag::Device as u64 != 0
    }

    fn page_prot(entry: u64) -> Prot {
        Prot {
            user: entry & PageFlag::User as u64 != 0,
            write: entry & PageFlag::Writable as u64 != 0,
            exec: entry & PageFlag::NoExecute as u64 == 0,
        }
    }

    fn kernel_root() -> u64 {
        boot::kernel_pml4()
    }

    fn init_root(root: u64) {
        phys::write_u64(root, boot::kernel_pml4_entry());
    }

    fn activate(root: u64) {
        if cpu::page_table() != root {
            cpu::set_page_table(root);
        }
    }
}
