// SPDX-License-Identifier: GPL-2.0

//! Physical memory access.
//!
//! Reads and writes allocator-owned physical frames through the identity map of the first 1 GiB.
//!
//! Only *managed* frames can be accessed: frames handed to the frame allocator at boot, which hold
//! page tables and user pages and nothing else. No Rust object ever lives in one (kernel code,
//! data, statics and stacks are all in the kernel image; boot-loader data stays reserved), and this
//! module never hands out references into them, only copies. That is what makes these functions
//! safe. Whether a page table *says* the right thing is [`crate::vmm`]'s business.

use core::sync::atomic::{AtomicU64, Ordering};

pub(crate) const PAGE_SIZE: u64 = 4096;
/// Top of the identity-mapped range; RAM above is unused.
pub(crate) const MAX_PHYS: u64 = 1 << 30;
const FRAMES: usize = (MAX_PHYS / PAGE_SIZE) as usize;
/// Below 1 MiB: BIOS data, VGA memory, boot-loader scratch space.
const LOW_MEMORY: u64 = 0x10_0000;

/// One bit per frame: set = managed.
static MANAGED: [AtomicU64; FRAMES / 64] = [const { AtomicU64::new(0) }; FRAMES / 64];

extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// The physical range occupied by the kernel image (code, data, bss).
pub(crate) fn kernel_image() -> (u64, u64) {
    (
        core::ptr::addr_of!(__kernel_start) as u64,
        core::ptr::addr_of!(__kernel_end) as u64,
    )
}

fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// Hands `frame` to the allocator.
///
/// # Panics
///
/// Panics for any frame that might hold something Rust code refers to: below
/// 1 MiB, in the kernel image, in boot-loader data, or above `MAX_PHYS`.
pub(crate) fn manage(frame: u64) {
    let range = (frame, frame + PAGE_SIZE);
    assert!(frame.is_multiple_of(PAGE_SIZE) && frame >= LOW_MEMORY && range.1 <= MAX_PHYS);
    assert!(
        !overlaps(range, kernel_image()),
        "frame {:#x} is part of the kernel",
        frame
    );
    assert!(
        super::multiboot::reserved_ranges().all(|r| !overlaps(range, r)),
        "frame {:#x} holds boot-loader data",
        frame
    );
    let f = (frame / PAGE_SIZE) as usize;
    MANAGED[f / 64].fetch_or(1 << (f % 64), Ordering::Relaxed);
}

pub(crate) fn is_managed(frame: u64) -> bool {
    let f = (frame / PAGE_SIZE) as usize;
    f < FRAMES && MANAGED[f / 64].load(Ordering::Relaxed) & (1 << (f % 64)) != 0
}

/// Returns whether `pml4` can be loaded into CR3 (the boot table or a managed frame).
pub(crate) fn is_page_table_root(pml4: u64) -> bool {
    pml4.is_multiple_of(PAGE_SIZE) && (pml4 == super::boot::kernel_pml4() || is_managed(pml4))
}

/// Checks that every frame of `[addr, addr + len)` is managed.
///
/// # Panics
///
/// Panics if one is not; every accessor below does the same.
fn check(addr: u64, len: usize) {
    let end = addr
        .checked_add(len as u64)
        .expect("physical range overflows");
    let mut f = addr & !(PAGE_SIZE - 1);
    while f < end {
        assert!(
            is_managed(f),
            "physical address {:#x} is not an allocator frame",
            f
        );
        f += PAGE_SIZE;
    }
}

pub(crate) fn read(addr: u64, buf: &mut [u8]) {
    check(addr, buf.len());
    // SAFETY: The source lies in managed frames (checked), which no Rust
    // reference points into, and `buf` is a distinct kernel buffer.
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), buf.len()) };
}

pub(crate) fn write(addr: u64, data: &[u8]) {
    check(addr, data.len());
    // SAFETY: As in `read`, in the other direction.
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
}

/// Copy between two managed ranges (`copy` semantics: may overlap).
pub(crate) fn copy(dst: u64, src: u64, len: usize) {
    check(dst, len);
    check(src, len);
    // SAFETY: Both ranges lie in managed frames (checked).
    unsafe { core::ptr::copy(src as *const u8, dst as *mut u8, len) };
}

pub(crate) fn zero(addr: u64, len: usize) {
    check(addr, len);
    // SAFETY: The range lies in managed frames (checked).
    unsafe { core::ptr::write_bytes(addr as *mut u8, 0, len) };
}

pub(crate) fn read_u64(addr: u64) -> u64 {
    let mut b = [0; 8];
    read(addr, &mut b);
    u64::from_ne_bytes(b)
}

pub(crate) fn write_u64(addr: u64, v: u64) {
    write(addr, &v.to_ne_bytes());
}
