// SPDX-License-Identifier: GPL-2.0

//! Virtual memory manager.
//!
//! Manages 4-level x86_64 page tables and all access to user memory.
//!
//! Address-space layout (every task):
//!
//! ```text
//! PML4[0]        kernel: identity map of the first 1 GiB (shared, supervisor only)
//! PML4[1..256]   user space (private per task, 4 KiB pages only)
//!
//! 0x0000_0080_0000_0000  USER_BASE       program image
//! 0x0000_0100_0000_0000  MMAP_BASE       anonymous mmap() region
//! 0x0000_7FFF_FFFF_F000  USER_STACK_TOP  (stack grows down)
//! ```
//!
//! Page tables and user pages are allocator frames, read and written through [`crate::arch::phys`].
//! User memory is only ever reached through the page tables of its address space (never by
//! dereferencing a user pointer), which also checks that it is mapped, user-accessible and, for
//! writes, writable.

use crate::arch::x86::PageFlag;
use crate::arch::{boot, cpu, phys};
use crate::pmm::{self, PAGE_SIZE};

/// The bits of a page-table entry that hold the physical address it points to.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// The bits of an address below its page: the offset in the page.
const PAGE_OFFSET: u64 = PAGE_SIZE - 1;
/// Entries per page table, at every level.
const TABLE_ENTRIES: usize = 512;
/// Bytes per page-table entry.
const ENTRY_SIZE: u64 = 8;
/// Address bits that each table level translates (log2 of [`TABLE_ENTRIES`]).
const INDEX_BITS: u32 = 9;
/// log2 of [`PAGE_SIZE`]: the address bits below the page-table index.
const PAGE_SHIFT: u32 = 12;
/// Table levels: 0 is the PML4, then the PDPT, the page directory and the page table.
const LEVELS: usize = 4;
/// The bits a task may not keep when its pages are copied: the CPU's
/// record of use, which starts afresh for the copy.
const USAGE_BITS: u64 = PageFlag::bits(&[PageFlag::Accessed, PageFlag::Dirty]);

pub(crate) const USER_BASE: u64 = 0x0000_0080_0000_0000;
pub(crate) const MMAP_BASE: u64 = 0x0000_0100_0000_0000;
pub(crate) const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;
pub(crate) const USER_LIMIT: u64 = haisuli_abi::USER_LIMIT;

/// PML4 slots holding user space: the lower half of the address space,
/// except slot 0, the shared kernel mapping.
const USER_SLOTS: core::ops::Range<usize> = 1..TABLE_ENTRIES / 2;

/// The boot PML4 doubles as the kernel's own address space.
pub(crate) fn kernel_pml4() -> u64 {
    boot::kernel_pml4()
}

fn entry(table: u64, i: usize) -> u64 {
    phys::read_u64(table + ENTRY_SIZE * i as u64)
}

fn set_entry(table: u64, i: usize, v: u64) {
    phys::write_u64(table + ENTRY_SIZE * i as u64, v)
}

/// Whether page-table entry (or entry flags) `e` has `flag`.
fn has(e: u64, flag: PageFlag) -> bool {
    e & flag as u64 != 0
}

/// Whether page-table entry `e` is in use.
fn present(e: u64) -> bool {
    has(e, PageFlag::Present)
}

/// The lowest address bit that the table at `level` translates.
const fn shift(level: usize) -> u32 {
    PAGE_SHIFT + INDEX_BITS * (LEVELS - 1 - level) as u32
}

/// Table index at `level` (see [`LEVELS`]).
fn idx(virt: u64, level: usize) -> usize {
    ((virt >> shift(level)) as usize) & (TABLE_ENTRIES - 1)
}

pub(crate) fn switch(pml4: u64) {
    cpu::set_page_table(pml4);
}

/// Create an address space that shares the kernel mapping and has no user pages.
pub(crate) fn new_address_space() -> Option<u64> {
    let f = pmm::alloc_zeroed()?;
    set_entry(f, 0, boot::kernel_pml4_entry());
    Some(f)
}

/// Maps one 4 KiB user page.
///
/// `flags` are [`PageFlag::User`], optionally with [`PageFlag::Writable`] and
/// [`PageFlag::NoExecute`]; [`PageFlag::Present`] is added.
///
/// # Panics
///
/// Panics if `frame` is not an allocator frame: user space must never see
/// kernel memory.
pub(crate) fn map_user(pml4: u64, virt: u64, frame: u64, flags: u64) -> Result<(), ()> {
    if !(USER_BASE..USER_LIMIT).contains(&virt) || virt & PAGE_OFFSET != 0 {
        return Err(());
    }
    // User space must never see kernel memory: only allocator frames.
    assert!(
        phys::is_managed(frame),
        "mapping a non-allocator frame {:#x} into user space",
        frame
    );
    // Intermediate tables allow everything; the leaf entry restricts.
    let table_entry = PageFlag::bits(&[PageFlag::Present, PageFlag::Writable, PageFlag::User]);
    let leaf = LEVELS - 1;
    let mut t = pml4;
    for lvl in 0..leaf {
        let e = entry(t, idx(virt, lvl));
        t = if !present(e) {
            let f = pmm::alloc_zeroed().ok_or(())?;
            set_entry(t, idx(virt, lvl), f | table_entry);
            f
        } else {
            e & ADDR_MASK
        };
    }
    if present(entry(t, idx(virt, leaf))) {
        return Err(());
    }
    set_entry(
        t,
        idx(virt, leaf),
        (frame & ADDR_MASK) | flags | PageFlag::Present as u64,
    );
    Ok(())
}

/// Allocates, fills and maps fresh user pages covering `[virt, virt + size)`.
///
/// `data` is placed at `virt` (which need not be page aligned); the rest is
/// zero. Fails if out of memory or if a page is already mapped; pages mapped
/// before the failure stay mapped (they are freed with the address space).
pub(crate) fn map_init(pml4: u64, virt: u64, size: u64, data: &[u8], flags: u64) -> Result<(), ()> {
    let end = virt.checked_add(size).ok_or(())?;
    if (data.len() as u64) > size {
        return Err(());
    }
    let data_end = virt + data.len() as u64;
    let mut page = virt & !PAGE_OFFSET;
    while page < end {
        let f = pmm::alloc_zeroed().ok_or(())?;
        // Part of `data` that falls into this page.
        let lo = page.max(virt);
        let hi = (page + PAGE_SIZE).min(data_end);
        if lo < hi {
            phys::write(
                f + (lo - page),
                &data[(lo - virt) as usize..(hi - virt) as usize],
            );
        }
        if map_user(pml4, page, f, flags).is_err() {
            pmm::free_frame(f);
            return Err(());
        }
        page += PAGE_SIZE;
    }
    Ok(())
}

/// Walks the page tables for a user address and returns its physical address and leaf flags.
pub(crate) fn translate(pml4: u64, virt: u64) -> Option<(u64, u64)> {
    if !(USER_BASE..USER_LIMIT).contains(&virt) {
        return None; // PML4[0] (kernel, 2 MiB pages) is not walked here.
    }
    let mut t = pml4;
    for lvl in 0..LEVELS - 1 {
        let e = entry(t, idx(virt, lvl));
        if !present(e) {
            return None;
        }
        t = e & ADDR_MASK;
    }
    let e = entry(t, idx(virt, LEVELS - 1));
    if !present(e) {
        return None;
    }
    Some(((e & ADDR_MASK) | (virt & PAGE_OFFSET), e & !ADDR_MASK))
}

/// Returns whether `[ptr, ptr + len)` is entirely mapped user memory (writable if `write`).
pub(crate) fn user_range_ok(pml4: u64, ptr: u64, len: u64, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    let end = match ptr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    if ptr < USER_BASE || end > USER_LIMIT {
        return false;
    }
    let mut a = ptr & !PAGE_OFFSET;
    while a < end {
        match translate(pml4, a) {
            Some((_, fl)) if has(fl, PageFlag::User) && (!write || has(fl, PageFlag::Writable)) => {
            }
            _ => return false,
        }
        a += PAGE_SIZE;
    }
    true
}

/// Physical address of user address `va`, which `user_range_ok` accepted.
fn user_phys(pml4: u64, va: u64) -> u64 {
    translate(pml4, va).expect("validated user page vanished").0
}

/// Bytes from `va` to the end of its page.
fn page_rest(va: u64) -> u64 {
    PAGE_SIZE - (va & (PAGE_SIZE - 1))
}

/// Copy `src` to user memory at `dst` in address space `pml4`.
pub(crate) fn copy_to_user(pml4: u64, dst: u64, src: &[u8]) -> bool {
    if !user_range_ok(pml4, dst, src.len() as u64, true) {
        return false;
    }
    let mut done = 0;
    while done < src.len() {
        let va = dst + done as u64;
        let n = (page_rest(va) as usize).min(src.len() - done);
        phys::write(user_phys(pml4, va), &src[done..done + n]);
        done += n;
    }
    true
}

/// Fill `dst` from user memory at `src` in address space `pml4`.
pub(crate) fn copy_from_user(pml4: u64, src: u64, dst: &mut [u8]) -> bool {
    if !user_range_ok(pml4, src, dst.len() as u64, false) {
        return false;
    }
    let mut done = 0;
    while done < dst.len() {
        let va = src + done as u64;
        let n = (page_rest(va) as usize).min(dst.len() - done);
        phys::read(user_phys(pml4, va), &mut dst[done..done + n]);
        done += n;
    }
    true
}

/// Copy `len` bytes of user memory from one address space to another.
pub(crate) fn copy_user_to_user(
    dst_pml4: u64,
    dst: u64,
    src_pml4: u64,
    src: u64,
    len: u64,
) -> bool {
    if !user_range_ok(src_pml4, src, len, false) || !user_range_ok(dst_pml4, dst, len, true) {
        return false;
    }
    let mut done = 0;
    while done < len {
        let (s, d) = (src + done, dst + done);
        let n = page_rest(s).min(page_rest(d)).min(len - done);
        phys::copy(user_phys(dst_pml4, d), user_phys(src_pml4, s), n as usize);
        done += n;
    }
    true
}

/// Call `f(virt, leaf entry)` for every mapped user page of `pml4`.
fn for_each_user_page(pml4: u64, mut f: impl FnMut(u64, u64)) {
    for i4 in USER_SLOTS {
        let e4 = entry(pml4, i4);
        if !present(e4) {
            continue;
        }
        for i3 in 0..TABLE_ENTRIES {
            let e3 = entry(e4 & ADDR_MASK, i3);
            if !present(e3) {
                continue;
            }
            for i2 in 0..TABLE_ENTRIES {
                let e2 = entry(e3 & ADDR_MASK, i2);
                if !present(e2) {
                    continue;
                }
                for i1 in 0..TABLE_ENTRIES {
                    let e1 = entry(e2 & ADDR_MASK, i1);
                    if present(e1) {
                        let virt = (i4 as u64) << shift(0)
                            | (i3 as u64) << shift(1)
                            | (i2 as u64) << shift(2)
                            | (i1 as u64) << shift(3);
                        f(virt, e1);
                    }
                }
            }
        }
    }
}

/// Deep-copy all user pages (used by `task_fork`; no copy-on-write yet).
pub(crate) fn clone_user_space(src: u64) -> Option<u64> {
    let dst = new_address_space()?;
    let mut ok = true;
    for_each_user_page(src, |virt, e| {
        if !ok {
            return;
        }
        let Some(f) = pmm::alloc_frame() else {
            ok = false;
            return;
        };
        phys::copy(f, e & ADDR_MASK, PAGE_SIZE as usize);
        let flags = e & !ADDR_MASK & !USAGE_BITS;
        if map_user(dst, virt, f, flags).is_err() {
            pmm::free_frame(f);
            ok = false;
        }
    });
    if !ok {
        destroy_user_space(dst);
        return None;
    }
    Some(dst)
}

/// Frees every user page and page table, then the PML4 itself.
///
/// Never call this on the active CR3.
pub(crate) fn destroy_user_space(pml4: u64) {
    for_each_user_page(pml4, |_, e| pmm::free_frame(e & ADDR_MASK));
    // Then the tables themselves, bottom up.
    for i4 in USER_SLOTS {
        let e4 = entry(pml4, i4);
        if !present(e4) {
            continue;
        }
        for i3 in 0..TABLE_ENTRIES {
            let e3 = entry(e4 & ADDR_MASK, i3);
            if !present(e3) {
                continue;
            }
            for i2 in 0..TABLE_ENTRIES {
                let e2 = entry(e3 & ADDR_MASK, i2);
                if present(e2) {
                    pmm::free_frame(e2 & ADDR_MASK);
                }
            }
            pmm::free_frame(e3 & ADDR_MASK);
        }
        pmm::free_frame(e4 & ADDR_MASK);
    }
    pmm::free_frame(pml4);
}
