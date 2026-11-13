// SPDX-License-Identifier: GPL-2.0

//! Virtual memory manager.
//!
//! Manages 4-level x86_64 page tables and all access to user memory.
//!
//! Address-space layout (every process):
//!
//! ```text
//! PML4[0]        kernel: identity map of the first 1 GiB (shared, supervisor only)
//! PML4[1..256]   user space (private per process, 4 KiB pages only)
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

use crate::arch::{boot, cpu, phys};
use crate::pmm::{self, PAGE_SIZE};

pub(crate) const PRESENT: u64 = 1;
pub(crate) const WRITABLE: u64 = 1 << 1;
pub(crate) const USER: u64 = 1 << 2;
pub(crate) const NX: u64 = 1 << 63;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

pub(crate) const USER_BASE: u64 = 0x0000_0080_0000_0000;
pub(crate) const MMAP_BASE: u64 = 0x0000_0100_0000_0000;
pub(crate) const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;
pub(crate) const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

/// PML4 slots holding user space (slot 0 is the shared kernel mapping).
const USER_SLOTS: core::ops::Range<usize> = 1..256;

/// The boot PML4 doubles as the kernel's own address space.
pub(crate) fn kernel_pml4() -> u64 {
    boot::kernel_pml4()
}

fn entry(table: u64, i: usize) -> u64 {
    phys::read_u64(table + 8 * i as u64)
}

fn set_entry(table: u64, i: usize, v: u64) {
    phys::write_u64(table + 8 * i as u64, v)
}

/// Table index at `level` (0 = PML4 ... 3 = PT).
fn idx(virt: u64, level: usize) -> usize {
    ((virt >> (39 - 9 * level)) & 0x1FF) as usize
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
/// `flags` are `USER`, optionally with `WRITABLE` and `NX`; `PRESENT` is added.
///
/// # Panics
///
/// Panics if `frame` is not an allocator frame: user space must never see
/// kernel memory.
pub(crate) fn map_user(pml4: u64, virt: u64, frame: u64, flags: u64) -> Result<(), ()> {
    if !(USER_BASE..USER_LIMIT).contains(&virt) || virt & 0xFFF != 0 {
        return Err(());
    }
    // User space must never see kernel memory: only allocator frames.
    assert!(
        phys::is_managed(frame),
        "mapping a non-allocator frame {:#x} into user space",
        frame
    );
    let mut t = pml4;
    for lvl in 0..3 {
        let e = entry(t, idx(virt, lvl));
        t = if e & PRESENT == 0 {
            let f = pmm::alloc_zeroed().ok_or(())?;
            set_entry(t, idx(virt, lvl), f | PRESENT | WRITABLE | USER);
            f
        } else {
            e & ADDR_MASK
        };
    }
    if entry(t, idx(virt, 3)) & PRESENT != 0 {
        return Err(());
    }
    set_entry(t, idx(virt, 3), (frame & ADDR_MASK) | flags | PRESENT);
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
    let mut page = virt & !(PAGE_SIZE - 1);
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
    for lvl in 0..3 {
        let e = entry(t, idx(virt, lvl));
        if e & PRESENT == 0 {
            return None;
        }
        t = e & ADDR_MASK;
    }
    let e = entry(t, idx(virt, 3));
    if e & PRESENT == 0 {
        return None;
    }
    Some(((e & ADDR_MASK) | (virt & 0xFFF), e & !ADDR_MASK))
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
    let mut a = ptr & !0xFFF;
    while a < end {
        match translate(pml4, a) {
            Some((_, fl)) if fl & USER != 0 && (!write || fl & WRITABLE != 0) => {}
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
        if e4 & PRESENT == 0 {
            continue;
        }
        for i3 in 0..512 {
            let e3 = entry(e4 & ADDR_MASK, i3);
            if e3 & PRESENT == 0 {
                continue;
            }
            for i2 in 0..512 {
                let e2 = entry(e3 & ADDR_MASK, i2);
                if e2 & PRESENT == 0 {
                    continue;
                }
                for i1 in 0..512 {
                    let e1 = entry(e2 & ADDR_MASK, i1);
                    if e1 & PRESENT != 0 {
                        let virt = ((i4 as u64) << 39)
                            | ((i3 as u64) << 30)
                            | ((i2 as u64) << 21)
                            | ((i1 as u64) << 12);
                        f(virt, e1);
                    }
                }
            }
        }
    }
}

/// Deep-copy all user pages (used by fork; no copy-on-write yet).
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
        let flags = e & !ADDR_MASK & !0x60; // Drop the accessed and dirty bits.
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
        if e4 & PRESENT == 0 {
            continue;
        }
        for i3 in 0..512 {
            let e3 = entry(e4 & ADDR_MASK, i3);
            if e3 & PRESENT == 0 {
                continue;
            }
            for i2 in 0..512 {
                let e2 = entry(e3 & ADDR_MASK, i2);
                if e2 & PRESENT != 0 {
                    pmm::free_frame(e2 & ADDR_MASK);
                }
            }
            pmm::free_frame(e3 & ADDR_MASK);
        }
        pmm::free_frame(e4 & ADDR_MASK);
    }
    pmm::free_frame(pml4);
}
