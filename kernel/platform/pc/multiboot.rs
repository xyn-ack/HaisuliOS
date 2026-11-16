// SPDX-License-Identifier: GPL-2.0

//! Multiboot information.
//!
//! Reads the memory map and boot modules that a Multiboot v1 boot loader left for the kernel.
//!
//! Everything it describes stays reserved for the kernel's lifetime (see [`reserved_ranges`]: the
//! allocator never gets it and [`crate::arch::phys`] refuses it), so it is never written after
//! boot and can be handed out as `&'static [u8]`.

use crate::arch::x86_64::x86::flags;
use core::sync::atomic::{AtomicU64, Ordering};

/// The magic numbers of the Multiboot v1 handshake.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Magic {
    /// Starts the kernel's Multiboot header, which the loader searches for.
    Header = 0x1BAD_B002,
    /// In eax on entry: the kernel was started by a Multiboot v1 loader.
    Loader = 0x2BAD_B002,
}

flags! {
    /// What the kernel's Multiboot header asks of the loader.
    pub(crate) enum HeaderFlag: u32 {
        /// Load modules at page-aligned addresses.
        PageAlignModules = 1 << 0,
        /// Provide the memory fields and the memory map.
        MemoryInfo = 1 << 1,
    }
}

/// The `flags` of the kernel's Multiboot header.
pub(crate) const HEADER_FLAGS: u32 =
    HeaderFlag::bits(&[HeaderFlag::PageAlignModules, HeaderFlag::MemoryInfo]);
/// The header's checksum: magic, flags and checksum add up to 0.
pub(crate) const HEADER_CHECKSUM: u32 = 0u32
    .wrapping_sub(Magic::Header as u32)
    .wrapping_sub(HEADER_FLAGS);

/// Bits of the information structure's `flags`: which fields are valid.
#[derive(Clone, Copy)]
#[repr(u32)]
enum InfoFlag {
    /// `mods_count` and `mods_addr`.
    Modules = 1 << 3,
    /// `mmap_length` and `mmap_addr`.
    MemoryMap = 1 << 6,
}

/// Offsets of the information structure's fields that the kernel reads.
#[derive(Clone, Copy)]
#[repr(u64)]
enum InfoField {
    /// `flags` (u32): see [`InfoFlag`].
    Flags = 0,
    /// `mods_count` (u32).
    ModsCount = 20,
    /// `mods_addr` (u32): the module list.
    ModsAddr = 24,
    /// `mmap_length` (u32): bytes in the memory map.
    MmapLength = 44,
    /// `mmap_addr` (u32): the memory map.
    MmapAddr = 48,
}

/// Offsets of the fields of a memory-map entry.
#[derive(Clone, Copy)]
#[repr(u64)]
enum MmapField {
    /// `size` (u32): bytes in the entry, not counting this field.
    Size = 0,
    /// `base_addr` (u64).
    Base = 4,
    /// `length` (u64).
    Length = 12,
    /// `type` (u32): see [`MemoryType`].
    Type = 20,
}

/// Offsets of the fields of a module-list entry.
#[derive(Clone, Copy)]
#[repr(u64)]
enum ModuleField {
    /// `mod_start` (u32).
    Start = 0,
    /// `mod_end` (u32).
    End = 4,
    /// `string` (u32): the module's command line.
    Cmdline = 8,
}

/// Bytes per module-list entry.
const MODULE_ENTRY_SIZE: u64 = 16;
/// The information structure, up to and including the fields we read.
const INFO_SIZE: u64 = 120;
/// Longest module command line that is read (without the NUL).
const CMDLINE_MAX: u64 = 255;

/// The type of a memory-map region.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryType {
    /// RAM the kernel may use.
    Available,
    /// Anything else: reserved, ACPI tables or NVS, defective RAM.
    Other(u32),
}

impl MemoryType {
    /// The `type` value of available RAM.
    const AVAILABLE: u32 = 1;

    fn from_raw(raw: u32) -> MemoryType {
        match raw {
            Self::AVAILABLE => MemoryType::Available,
            other => MemoryType::Other(other),
        }
    }
}

/// Physical address of the information structure (0 = none).
static INFO: AtomicU64 = AtomicU64::new(0);

/// Record where the boot loader put its information structure.
///
/// # Safety
/// `mb_info` must be the address a Multiboot v1 loader passed in ebx, and the
/// memory it describes must not have been modified since.
pub(crate) unsafe fn init(mb_info: u64) {
    INFO.store(mb_info, Ordering::Relaxed);
}

/// Boot-loader memory as a byte slice.
fn bytes(addr: u64, len: u64) -> &'static [u8] {
    assert!(
        addr != 0
            && addr
                .checked_add(len)
                .is_some_and(|end| end <= crate::arch::phys::MAX_PHYS)
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

/// The information structure, if the loader provided the fields of `flag`.
fn has(flag: InfoFlag) -> Option<u64> {
    info().filter(|&i| u32_at(i + InfoField::Flags as u64) & flag as u32 != 0)
}

/// The u32 field `field` of the information structure at `info`, widened.
fn info_field(info: u64, field: InfoField) -> u64 {
    u32_at(info + field as u64) as u64
}

/// A region of the boot loader's memory map: (base, length, type).
pub(crate) fn memory_map() -> impl Iterator<Item = (u64, u64, MemoryType)> {
    let (mut p, end) = match has(InfoFlag::MemoryMap) {
        Some(i) => {
            let p = info_field(i, InfoField::MmapAddr);
            (p, p + info_field(i, InfoField::MmapLength))
        }
        None => (0, 0),
    };
    core::iter::from_fn(move || {
        if p >= end {
            return None;
        }
        let entry = (
            u64_at(p + MmapField::Base as u64),
            u64_at(p + MmapField::Length as u64),
            MemoryType::from_raw(u32_at(p + MmapField::Type as u64)),
        );
        // `size` does not count itself.
        p += MmapField::Base as u64 + u32_at(p + MmapField::Size as u64) as u64;
        Some(entry)
    })
}

pub(crate) fn has_memory_map() -> bool {
    has(InfoFlag::MemoryMap).is_some()
}

/// Raw module entries: (start, end, cmdline address).
fn module_entries() -> impl Iterator<Item = (u64, u64, u64)> {
    let (count, list) = match has(InfoFlag::Modules) {
        Some(i) => (
            info_field(i, InfoField::ModsCount),
            info_field(i, InfoField::ModsAddr),
        ),
        None => (0, 0),
    };
    (0..count).map(move |n| {
        let e = list + MODULE_ENTRY_SIZE * n;
        let field = |f: ModuleField| u32_at(e + f as u64) as u64;
        (
            field(ModuleField::Start),
            field(ModuleField::End),
            field(ModuleField::Cmdline),
        )
    })
}

/// Length of the NUL-terminated string at `addr` (at most [`CMDLINE_MAX`] bytes).
fn cstr_len(addr: u64) -> u64 {
    (0..CMDLINE_MAX)
        .find(|&n| bytes(addr + n, 1)[0] == 0)
        .unwrap_or(CMDLINE_MAX)
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
    let mmap = has(InfoFlag::MemoryMap).map(|i| {
        let p = info_field(i, InfoField::MmapAddr);
        (p, p + info_field(i, InfoField::MmapLength))
    });
    let list = has(InfoFlag::Modules).map(|i| {
        let p = info_field(i, InfoField::ModsAddr);
        (
            p,
            p + MODULE_ENTRY_SIZE * info_field(i, InfoField::ModsCount),
        )
    });
    let mods = module_entries().flat_map(|(start, end, cmdline)| {
        let cmd = (cmdline != 0).then(|| (cmdline, cmdline + cstr_len(cmdline) + 1));
        core::iter::once((start, end)).chain(cmd)
    });
    info.into_iter().chain(mmap).chain(list).chain(mods)
}
