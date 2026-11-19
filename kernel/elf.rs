// SPDX-License-Identifier: MIT

//! ELF loader.
//!
//! Loads static ELF64 executables (`ET_EXEC`) for this CPU ([`ELF_MACHINE`]) into a user address
//! space.
//!
//! Every `PT_LOAD` segment is copied into freshly allocated pages of the target address space with
//! matching permissions: writable only if `PF_W`, no-execute unless `PF_X`. Segments that are both
//! writable and executable are refused (W^X), as are segments outside the program-image region
//! `[USER_BASE, MMAP_BASE)` and segments sharing a page with another segment.

use crate::arch::{Prot, ELF_MACHINE};
use crate::vmm::{self, AddressSpace};

/// The file starts with these four bytes (`e_ident[EI_MAG0..4]`).
const MAGIC: &[u8; 4] = b"\x7fELF";
/// `e_ident[EI_CLASS]` of a 64-bit file.
const CLASS_64: u8 = 2;
/// `e_ident[EI_DATA]` of a little-endian file.
const DATA_LITTLE_ENDIAN: u8 = 1;
/// `e_type` of a (static) executable, `ET_EXEC`.
const TYPE_EXECUTABLE: u16 = 2;
/// `p_type` of a segment to load, `PT_LOAD`.
const SEGMENT_LOAD: u32 = 1;

/// Offsets of the ELF64 file header's fields that the loader reads.
#[derive(Clone, Copy)]
#[repr(usize)]
enum Header {
    /// `e_ident[EI_CLASS]` (u8).
    Class = 4,
    /// `e_ident[EI_DATA]` (u8).
    Data = 5,
    /// `e_type` (u16).
    Type = 16,
    /// `e_machine` (u16).
    Machine = 18,
    /// `e_entry` (u64): the entry point.
    Entry = 24,
    /// `e_phoff` (u64): where the program headers start.
    ProgramHeaders = 32,
    /// `e_phentsize` (u16): bytes per program header.
    ProgramHeaderSize = 54,
    /// `e_phnum` (u16): number of program headers.
    ProgramHeaderCount = 56,
}

/// Offsets of the ELF64 program header's fields.
#[derive(Clone, Copy)]
#[repr(usize)]
enum ProgramHeader {
    /// `p_type` (u32).
    Type = 0,
    /// `p_flags` (u32): see [`SegmentFlag`].
    Flags = 4,
    /// `p_offset` (u64): where the segment's bytes are in the file.
    Offset = 8,
    /// `p_vaddr` (u64).
    VirtualAddress = 16,
    /// `p_filesz` (u64): bytes in the file.
    FileSize = 32,
    /// `p_memsz` (u64): bytes in memory (the rest is zero).
    MemorySize = 40,
}

/// The smallest program header that has every field the loader reads.
const PROGRAM_HEADER_MIN: usize = 56;

/// `p_flags` bits: the segment's permissions.
#[derive(Clone, Copy)]
#[repr(u32)]
enum SegmentFlag {
    /// `PF_X`: executable.
    Execute = 1 << 0,
    /// `PF_W`: writable.
    Write = 1 << 1,
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

pub(crate) fn is_elf(image: &[u8]) -> bool {
    image.starts_with(MAGIC)
}

struct Segment {
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

fn segment(image: &[u8], ph: usize) -> Option<(u32, Segment)> {
    let at = |field: ProgramHeader| ph + field as usize;
    Some((
        u32_at(image, at(ProgramHeader::Type))?,
        Segment {
            flags: u32_at(image, at(ProgramHeader::Flags))?,
            offset: u64_at(image, at(ProgramHeader::Offset))?,
            vaddr: u64_at(image, at(ProgramHeader::VirtualAddress))?,
            filesz: u64_at(image, at(ProgramHeader::FileSize))?,
            memsz: u64_at(image, at(ProgramHeader::MemorySize))?,
        },
    ))
}

impl Segment {
    fn has(&self, flag: SegmentFlag) -> bool {
        self.flags & flag as u32 != 0
    }
}

/// Loads `image` into the (empty) user half of `space` and returns its entry point.
///
/// On error the address space may be partially populated; the caller
/// destroys it.
pub(crate) fn load(space: AddressSpace, image: &[u8]) -> Result<u64, &'static str> {
    const BAD: &str = "malformed ELF header";
    let byte = |field: Header| image.get(field as usize).copied();
    if !is_elf(image)
        || byte(Header::Class) != Some(CLASS_64)
        || byte(Header::Data) != Some(DATA_LITTLE_ENDIAN)
    {
        return Err("not a 64-bit little-endian ELF file");
    }
    let u16_field = |field: Header| u16_at(image, field as usize).ok_or(BAD);
    let u64_field = |field: Header| u64_at(image, field as usize).ok_or(BAD);
    if u16_field(Header::Type)? != TYPE_EXECUTABLE || u16_field(Header::Machine)? != ELF_MACHINE {
        return Err("not a static executable for this CPU");
    }
    let entry = u64_field(Header::Entry)?;
    let phoff = u64_field(Header::ProgramHeaders)? as usize;
    let phentsize = u16_field(Header::ProgramHeaderSize)? as usize;
    let phnum = u16_field(Header::ProgramHeaderCount)? as usize;
    if phentsize < PROGRAM_HEADER_MIN {
        return Err(BAD);
    }

    let mut entry_ok = false;
    let mut loaded = 0;
    for i in 0..phnum {
        let ph = phoff.checked_add(i * phentsize).ok_or(BAD)?;
        let (ty, s) = segment(image, ph).ok_or("truncated program header")?;
        if ty != SEGMENT_LOAD || s.memsz == 0 {
            continue;
        }
        if s.has(SegmentFlag::Write) && s.has(SegmentFlag::Execute) {
            return Err("segment is both writable and executable");
        }
        let in_image = s.vaddr >= vmm::USER_BASE
            && s.vaddr
                .checked_add(s.memsz)
                .is_some_and(|end| end <= vmm::MMAP_BASE);
        if !in_image || s.filesz > s.memsz {
            return Err("segment outside the program image region");
        }
        let file_end = s.offset.checked_add(s.filesz).ok_or(BAD)?;
        let data = image
            .get(s.offset as usize..file_end as usize)
            .ok_or("segment past end of file")?;

        let prot = Prot {
            user: true,
            write: s.has(SegmentFlag::Write),
            exec: s.has(SegmentFlag::Execute),
        };
        if prot.exec && (s.vaddr..s.vaddr + s.memsz).contains(&entry) {
            entry_ok = true;
        }
        vmm::map_init(space, s.vaddr, s.memsz, data, prot)
            .map_err(|_| "out of memory, or segments share a page")?;
        loaded += 1;
    }
    if loaded == 0 {
        return Err("no loadable segments");
    }
    if !entry_ok {
        return Err("entry point is not in an executable segment");
    }
    Ok(entry)
}
