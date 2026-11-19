// SPDX-License-Identifier: MIT

//! ACPI tables.
//!
//! Finds the CPUs of the machine in the firmware's ACPI tables: the local APIC ids that the
//! MADT lists as enabled.
//!
//! The tables are firmware memory: the BIOS area below 1 MiB and ACPI regions of the memory map.
//! The frame allocator never owns them and the kernel never writes them, so they can be read as
//! `&'static [u8]`. [`firmware`] only hands out ranges that are identity mapped and that no
//! allocator frame or kernel data overlaps; every table is checked (signature, length and
//! checksum) before it is used.

use crate::arch::phys::{self, MAX_PHYS, PAGE_SIZE};
use crate::platform::CpuIds;

/// Where the BIOS data area keeps the segment of the extended BIOS data area.
const EBDA_SEGMENT_POINTER: u64 = 0x40E;
/// The EBDA lies in conventional memory, below the video memory at 640 KiB.
const EBDA_RANGE: core::ops::Range<u64> = 0x8_0000..0xA_0000;
/// The first KiB of the EBDA and the BIOS ROM area are where the RSDP may be (ACPI 5.2.5.1).
const EBDA_SEARCH_LEN: u64 = 1024;
const BIOS_AREA: core::ops::Range<u64> = 0xE_0000..0x10_0000;
/// The RSDP's signature, and the bytes covered by its ACPI 1.0 checksum.
const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";
const RSDP_V1_LEN: u64 = 20;
/// The RSDP field with the RSDT's physical address (u32).
const RSDP_RSDT: usize = 16;
/// Bytes in the header that every system description table starts with.
const HEADER_LEN: usize = 36;
/// The header's length field (u32).
const HEADER_LENGTH: usize = 4;
/// The MADT: "Multiple APIC Description Table".
const MADT_SIGNATURE: &[u8; 4] = b"APIC";
/// The MADT's interrupt controller structures start after the header, the local APIC address
/// and the flags.
const MADT_ENTRIES: usize = HEADER_LEN + 8;
/// Interrupt controller structure: a processor's local APIC, and its fields.
const MADT_LOCAL_APIC: u8 = 0;
const LOCAL_APIC_ID: usize = 3;
const LOCAL_APIC_FLAGS: usize = 4;
/// Local APIC flags: the processor is enabled.
const LOCAL_APIC_ENABLED: u32 = 1;

/// `len` bytes of firmware memory at `addr`, if that is memory the kernel may read as such.
fn firmware(addr: u64, len: u64) -> Option<&'static [u8]> {
    let end = addr.checked_add(len)?;
    if addr == 0 || end > MAX_PHYS {
        return None;
    }
    let (kernel_start, kernel_end) = phys::kernel_image();
    if addr < kernel_end && kernel_start < end {
        return None;
    }
    let mut page = addr & !(PAGE_SIZE - 1);
    while page < end {
        if phys::is_managed(page) {
            return None;
        }
        page += PAGE_SIZE;
    }
    // SAFETY: The range is identity mapped (below `MAX_PHYS`), outside the kernel image and
    // outside every allocator frame, so no Rust object lives there and the kernel never writes
    // it: it is firmware memory (see the module documentation), which stays unchanged.
    Some(unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) })
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Whether the bytes add up to 0, as every ACPI checksum requires.
fn checksum_ok(b: &[u8]) -> bool {
    b.iter().fold(0u8, |sum, &x| sum.wrapping_add(x)) == 0
}

/// The RSDP, found in the first KiB of the EBDA or in the BIOS area, on 16-byte boundaries.
fn rsdp() -> Option<&'static [u8]> {
    let ebda = firmware(EBDA_SEGMENT_POINTER, 2)
        .map(|b| (u16::from_le_bytes([b[0], b[1]]) as u64) << 4)
        .filter(|a| EBDA_RANGE.contains(a));
    let ebda = ebda.map(|a| a..(a + EBDA_SEARCH_LEN).min(EBDA_RANGE.end));
    ebda.into_iter()
        .chain(core::iter::once(BIOS_AREA))
        .flat_map(|range| range.step_by(16))
        .filter_map(|a| firmware(a, RSDP_V1_LEN))
        .find(|b| b.starts_with(RSDP_SIGNATURE) && checksum_ok(b))
}

/// The system description table at `addr`, if it is a valid one.
fn table(addr: u64) -> Option<&'static [u8]> {
    let header = firmware(addr, HEADER_LEN as u64)?;
    let len = u32_at(header, HEADER_LENGTH) as u64;
    if len < HEADER_LEN as u64 {
        return None;
    }
    firmware(addr, len).filter(|t| checksum_ok(t))
}

/// The local APIC ids of the enabled CPUs, in the MADT's order (at most
/// [`MAX_CPUS`](crate::arch::percpu::MAX_CPUS)).
///
/// Empty if the firmware has no (valid) MADT: then the kernel runs on the boot CPU alone.
pub(crate) fn cpus() -> CpuIds {
    let mut ids = CpuIds::new();
    let Some(rsdt) = rsdp().and_then(|r| table(u32_at(r, RSDP_RSDT) as u64)) else {
        return ids;
    };
    let madt = rsdt[HEADER_LEN..]
        .as_chunks::<4>()
        .0
        .iter()
        .filter_map(|p| table(u32::from_le_bytes(*p) as u64))
        .find(|t| t.starts_with(MADT_SIGNATURE));
    let Some(madt) = madt else {
        return ids;
    };
    let mut at = MADT_ENTRIES;
    while at + 2 <= madt.len() {
        let (kind, len) = (madt[at], madt[at + 1] as usize);
        if len < 2 || at + len > madt.len() {
            break;
        }
        if kind == MADT_LOCAL_APIC
            && len >= LOCAL_APIC_FLAGS + 4
            && u32_at(madt, at + LOCAL_APIC_FLAGS) & LOCAL_APIC_ENABLED != 0
            && !ids.push(madt[at + LOCAL_APIC_ID] as u64)
        {
            break; // `MAX_CPUS` of them.
        }
        at += len;
    }
    ids
}
