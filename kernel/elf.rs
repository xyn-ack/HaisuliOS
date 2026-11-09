// SPDX-License-Identifier: GPL-2.0

//! ELF loader.
//!
//! Loads static x86_64 ELF64 executables (`ET_EXEC`) into a user address space.
//!
//! Every `PT_LOAD` segment is copied into freshly allocated pages of the target address space with
//! matching permissions: writable only if `PF_W`, no-execute unless `PF_X`. Segments that are both
//! writable and executable are refused (W^X), as are segments outside the program-image region
//! `[USER_BASE, MMAP_BASE)` and segments sharing a page with another segment.

use crate::vmm;

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;

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
    image.starts_with(b"\x7fELF")
}

struct Segment {
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

fn segment(image: &[u8], ph: usize) -> Option<(u32, Segment)> {
    Some((
        u32_at(image, ph)?,
        Segment {
            flags: u32_at(image, ph + 4)?,
            offset: u64_at(image, ph + 8)?,
            vaddr: u64_at(image, ph + 16)?,
            filesz: u64_at(image, ph + 32)?,
            memsz: u64_at(image, ph + 40)?,
        },
    ))
}

/// Loads `image` into the (empty) user half of `pml4` and returns its entry point.
///
/// On error the address space may be partially populated; the caller
/// destroys it.
pub(crate) fn load(pml4: u64, image: &[u8]) -> Result<u64, &'static str> {
    const BAD: &str = "malformed ELF header";
    if !is_elf(image) || image.get(4) != Some(&2) || image.get(5) != Some(&1) {
        return Err("not a 64-bit little-endian ELF file");
    }
    if u16_at(image, 16).ok_or(BAD)? != ET_EXEC || u16_at(image, 18).ok_or(BAD)? != EM_X86_64 {
        return Err("not a static x86_64 executable");
    }
    let entry = u64_at(image, 24).ok_or(BAD)?;
    let phoff = u64_at(image, 32).ok_or(BAD)? as usize;
    let phentsize = u16_at(image, 54).ok_or(BAD)? as usize;
    let phnum = u16_at(image, 56).ok_or(BAD)? as usize;
    if phentsize < 56 {
        return Err(BAD);
    }

    let mut entry_ok = false;
    let mut loaded = 0;
    for i in 0..phnum {
        let ph = phoff.checked_add(i * phentsize).ok_or(BAD)?;
        let (ty, s) = segment(image, ph).ok_or("truncated program header")?;
        if ty != PT_LOAD || s.memsz == 0 {
            continue;
        }
        if s.flags & PF_W != 0 && s.flags & PF_X != 0 {
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

        let mut flags = vmm::USER;
        if s.flags & PF_W != 0 {
            flags |= vmm::WRITABLE;
        }
        if s.flags & PF_X == 0 {
            flags |= vmm::NX;
        } else if (s.vaddr..s.vaddr + s.memsz).contains(&entry) {
            entry_ok = true;
        }
        vmm::map_init(pml4, s.vaddr, s.memsz, data, flags)
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
