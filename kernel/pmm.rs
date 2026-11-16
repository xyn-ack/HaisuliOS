// SPDX-License-Identifier: GPL-2.0

//! Physical memory manager.
//!
//! A bitmap frame allocator over the first 1 GiB, the range identity-mapped by the boot page
//! tables.

use crate::arch::multiboot::{self, MemoryType};
use crate::arch::phys;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub(crate) use crate::arch::phys::{MAX_PHYS, PAGE_SIZE};
const FRAMES: usize = (MAX_PHYS / PAGE_SIZE) as usize;

/// One bit per frame: set if the frame is in use or unavailable.
static BITMAP: [AtomicU64; FRAMES / 64] = [const { AtomicU64::new(u64::MAX) }; FRAMES / 64];
static FREE: AtomicUsize = AtomicUsize::new(0);

fn bit(f: usize) -> (&'static AtomicU64, u64) {
    (&BITMAP[f / 64], 1 << (f % 64))
}

fn mark_free(f: usize) {
    let (word, mask) = bit(f);
    if word.fetch_and(!mask, Ordering::Relaxed) & mask != 0 {
        FREE.fetch_add(1, Ordering::Relaxed);
    }
}

fn mark_used(f: usize) {
    let (word, mask) = bit(f);
    if word.fetch_or(mask, Ordering::Relaxed) & mask == 0 {
        FREE.fetch_sub(1, Ordering::Relaxed);
    }
}

fn reserve(from: u64, to: u64) {
    let mut a = from & !(PAGE_SIZE - 1);
    while a < to && a < MAX_PHYS {
        mark_used((a / PAGE_SIZE) as usize);
        a += PAGE_SIZE;
    }
}

/// Populate the allocator from the Multiboot memory map.
pub(crate) fn init() {
    for (base, len, kind) in multiboot::memory_map() {
        if kind != MemoryType::Available {
            continue;
        }
        let start = (base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let stop = ((base + len) & !(PAGE_SIZE - 1)).min(MAX_PHYS);
        let mut a = start;
        while a < stop {
            mark_free((a / PAGE_SIZE) as usize);
            a += PAGE_SIZE;
        }
    }
    reserve(0, phys::LOW_MEMORY); // BIOS, VGA, boot-loader scratch space, frame 0.
    let (start, end) = phys::kernel_image();
    reserve(start, end);
    // Boot modules (user programs, ramdisk) and the boot information that
    // describes them stay in use: keep them intact.
    for (start, end) in multiboot::reserved_ranges() {
        reserve(start, end);
    }
    // Everything still free becomes allocator-owned memory.
    for f in 0..FRAMES {
        let (word, mask) = bit(f);
        if word.load(Ordering::Relaxed) & mask == 0 {
            phys::manage(f as u64 * PAGE_SIZE);
        }
    }
    crate::println!(
        "[pmm] {} frames free ({} MiB)",
        free_frames(),
        free_frames() as u64 * PAGE_SIZE / (1 << 20)
    );
}

pub(crate) fn free_frames() -> usize {
    FREE.load(Ordering::Relaxed)
}

/// Takes a free frame. Several CPUs may allocate at once: a frame is only theirs if their
/// `fetch_or` is the one that set its bit.
pub(crate) fn alloc_frame() -> Option<u64> {
    for (w, word) in BITMAP.iter().enumerate() {
        let mut bits = word.load(Ordering::Relaxed);
        while bits != u64::MAX {
            let bit = (!bits).trailing_zeros() as usize;
            let old = word.fetch_or(1 << bit, Ordering::AcqRel);
            if old & (1 << bit) == 0 {
                FREE.fetch_sub(1, Ordering::Relaxed);
                return Some(((w * 64 + bit) as u64) * PAGE_SIZE);
            }
            bits = old | (1 << bit); // Another CPU took it first: try the next one.
        }
    }
    None
}

pub(crate) fn alloc_zeroed() -> Option<u64> {
    let f = alloc_frame()?;
    phys::zero(f, PAGE_SIZE as usize);
    Some(f)
}

/// Returns `frame` to the allocator.
///
/// # Panics
///
/// Panics if the allocator does not own `frame`.
pub(crate) fn free_frame(frame: u64) {
    assert!(
        phys::is_managed(frame),
        "freeing a frame the allocator does not own: {:#x}",
        frame
    );
    mark_free((frame / PAGE_SIZE) as usize);
}
