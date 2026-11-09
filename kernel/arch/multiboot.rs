// SPDX-License-Identifier: GPL-2.0

//! Multiboot information.
//!
//! Reads the memory map and boot modules that a Multiboot v1 boot loader left for the kernel.
//!
//! Everything it describes stays reserved for the kernel's lifetime (see [`reserved_ranges`]: the
//! allocator never gets it and [`super::phys`] refuses it), so it is never written after boot and
//! can be handed out as `&'static [u8]`.

use core::sync::atomic::{AtomicU64, Ordering};

/// Information-structure `flags` bits.
const FLAG_MODS: u32 = 1 << 3;
const FLAG_MMAP: u32 = 1 << 6;
/// The information structure, up to and including the fields we read.
const INFO_SIZE: u64 = 120;
/// Value of eax on entry from a Multiboot v1 loader.
pub(crate) const BOOTLOADER_MAGIC: u32 = 0x2BAD_B002;

/// Physical address of the information structure (0 = none).
static INFO: AtomicU64 = AtomicU64::new(0);

/// Record where the boot loader put its information structure.
///
/// # Safety
/// `mb_info` must be the address a Multiboot v1 loader passed in ebx, and the
/// memory it describes must not have been modified since.
pub(super) unsafe fn init(mb_info: u64) {
    INFO.store(mb_info, Ordering::Relaxed);
}

/// Boot-loader memory as a byte slice.
fn bytes(addr: u64, len: u64) -> &'static [u8] {
    assert!(
        addr != 0
            && addr
                .checked_add(len)
                .is_some_and(|end| end <= super::phys::MAX_PHYS)
    );
    // SAFETY: Callers only pass ranges taken from the information structure,
    // which `init`'s contract guarantees to be valid; all of them are listed
    // by `reserved_ranges` and therefore never written (see module docs).
    unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) }
}

fn u32_at(addr: u64) -> u32 {
    u32::from_le_bytes(bytes(addr, 4).try_into().unwrap())
}

fn u64_at(addr: u64) -> u64 {
    u64::from_le_bytes(bytes(addr, 8).try_into().unwrap())
}

fn info() -> Option<u64> {
    match INFO.load(Ordering::Relaxed) {
        0 => None,
        a => Some(a),
    }
}

fn has(flag: u32) -> Option<u64> {
    info().filter(|&i| u32_at(i) & flag != 0)
}

/// A region of the boot loader's memory map: (base, length, type); type 1
/// is usable RAM.
pub(crate) fn memory_map() -> impl Iterator<Item = (u64, u64, u32)> {
    let (mut p, end) = match has(FLAG_MMAP) {
        Some(i) => {
            let p = u32_at(i + 48) as u64;
            (p, p + u32_at(i + 44) as u64)
        }
        None => (0, 0),
    };
    core::iter::from_fn(move || {
        if p >= end {
            return None;
        }
        let entry = (u64_at(p + 4), u64_at(p + 12), u32_at(p + 20));
        p += u32_at(p) as u64 + 4;
        Some(entry)
    })
}

pub(crate) fn has_memory_map() -> bool {
    has(FLAG_MMAP).is_some()
}

/// Raw module entries: (start, end, cmdline address).
fn module_entries() -> impl Iterator<Item = (u64, u64, u64)> {
    let (count, list) = match has(FLAG_MODS) {
        Some(i) => (u32_at(i + 20) as u64, u32_at(i + 24) as u64),
        None => (0, 0),
    };
    (0..count).map(move |n| {
        let e = list + 16 * n;
        (u32_at(e) as u64, u32_at(e + 4) as u64, u32_at(e + 8) as u64)
    })
}

/// Length of the NUL-terminated string at `addr` (at most 255 bytes).
fn cstr_len(addr: u64) -> u64 {
    (0..255)
        .find(|&n| bytes(addr + n, 1)[0] == 0)
        .unwrap_or(255)
}

/// A boot module (a file loaded next to the kernel, e.g. with GRUB's
/// `module` command or QEMU's `-initrd`).
#[derive(Clone, Copy)]
pub(crate) struct Module {
    pub data: &'static [u8],
    /// The module's command line (GRUB: `module /boot/x.elf <cmdline>`).
    pub cmdline: &'static [u8],
}

impl Module {
    /// A short name: the last path component of the command line's first word
    /// (QEMU passes the file name there, GRUB whatever follows the path).
    pub(crate) fn name(&self) -> &'static [u8] {
        let first = self.cmdline.split(|&b| b == b' ').next().unwrap_or(b"");
        first.rsplit(|&b| b == b'/').next().unwrap_or(b"")
    }
}

pub(crate) fn modules() -> impl Iterator<Item = Module> {
    module_entries().map(|(start, end, cmdline)| Module {
        data: if end > start {
            bytes(start, end - start)
        } else {
            &[]
        },
        cmdline: if cmdline != 0 {
            bytes(cmdline, cstr_len(cmdline))
        } else {
            b""
        },
    })
}

/// Every physical range holding boot-loader data that is still in use:
/// the information structure, memory map, module list, modules and their
/// command lines.
pub(crate) fn reserved_ranges() -> impl Iterator<Item = (u64, u64)> {
    let info = info().map(|i| (i, i + INFO_SIZE));
    let mmap = has(FLAG_MMAP).map(|i| {
        let p = u32_at(i + 48) as u64;
        (p, p + u32_at(i + 44) as u64)
    });
    let list = has(FLAG_MODS).map(|i| {
        let p = u32_at(i + 24) as u64;
        (p, p + 16 * u32_at(i + 20) as u64)
    });
    let mods = module_entries().flat_map(|(start, end, cmdline)| {
        let cmd = (cmdline != 0).then(|| (cmdline, cmdline + cstr_len(cmdline) + 1));
        core::iter::once((start, end)).chain(cmd)
    });
    info.into_iter().chain(mmap).chain(list).chain(mods)
}
