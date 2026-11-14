// SPDX-License-Identifier: GPL-2.0

//! Multiboot entry.
//!
//! Switches from the loader's 32-bit protected mode to 64-bit long mode and enters Rust.
//!
//! The assembly builds an identity map of the first 1 GiB (2 MiB pages), enables PAE, long mode and
//! NX, loads a temporary GDT and calls `boot_entry`, which calls [`crate::kmain`].
//!
//! These page tables stay in use as the *kernel half* of every address space: PML4\[0\] is shared by
//! all tasks, and user space lives in PML4\[1..256\]. The top-level table is a Rust static so the
//! rest of the kernel can read it without unsafe code; the assembly refers to it (and to the Rust
//! entry point) with `sym` operands.

use super::multiboot::{self, Magic, HEADER_CHECKSUM, HEADER_FLAGS};
use super::x86::{segment_descriptor, Cr0, Cr4, Efer, Msr, PageFlag, SegmentAccess, SegmentFlag};
use crate::gdt::Selector;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

#[repr(C, align(4096))]
pub(crate) struct PageTable(pub [AtomicU64; 512]);

/// The boot PML4, which doubles as the kernel's own address space.
pub(crate) static BOOT_PML4: PageTable = PageTable([const { AtomicU64::new(0) }; 512]);

/// Physical (= virtual) address of the boot PML4.
pub(crate) fn kernel_pml4() -> u64 {
    &BOOT_PML4 as *const PageTable as u64
}

/// PML4 entry 0: the kernel's identity map, shared by every address space.
pub(crate) fn kernel_pml4_entry() -> u64 {
    BOOT_PML4.0[0].load(Ordering::Relaxed)
}

/// First Rust code, on the boot stack in 64-bit mode.
extern "C" fn boot_entry(mb_info: u32, magic: u32) -> ! {
    if magic == Magic::Loader as u32 {
        // SAFETY: A Multiboot loader passed `mb_info` in ebx, and the boot
        // code above has not touched memory outside the kernel image.
        unsafe { multiboot::init(mb_info as u64) };
    }
    crate::kmain()
}

/// Page-table entries pointing at the next-level tables.
const TABLE_ENTRY: u64 = PageFlag::bits(&[PageFlag::Present, PageFlag::Writable]);
/// Page-directory entries mapping 2 MiB of kernel memory each.
const HUGE_PAGE_ENTRY: u64 =
    PageFlag::bits(&[PageFlag::Present, PageFlag::Writable, PageFlag::Huge]);
/// log2 of the 2 MiB that a page-directory entry maps.
const HUGE_PAGE_SHIFT: u32 = 21;
/// Entries per page table.
const TABLE_ENTRIES: u32 = 512;

/// The temporary GDT's code segment: 64-bit kernel code (long mode ignores
/// the base and limit; the GDT proper comes from [`crate::gdt`]).
const BOOT_CODE_SEGMENT: u64 = segment_descriptor(
    SegmentAccess::bits(&[
        SegmentAccess::Present,
        SegmentAccess::CodeOrData,
        SegmentAccess::Executable,
        SegmentAccess::ReadWrite,
    ]),
    SegmentFlag::LongMode as u8,
    0,
);
/// The temporary GDT's data segment.
const BOOT_DATA_SEGMENT: u64 = segment_descriptor(
    SegmentAccess::bits(&[
        SegmentAccess::Present,
        SegmentAccess::CodeOrData,
        SegmentAccess::ReadWrite,
    ]),
    0,
    0,
);

// The boot GDT (null, code, data, in that order below) has the kernel's code
// and data segments at the same selectors as the GDT proper.
const _: () = assert!(Selector::KernelCode.index() == 1 && Selector::KernelData.index() == 2);

global_asm!(
    r#"
.pushsection .multiboot, "a"
.balign 4
.long {mb_magic}
.long {mb_flags}
.long {mb_checksum}
.popsection

.pushsection .bss.boot, "aw", @nobits
.balign 4096
boot_pdpt: .skip 4096
boot_pd:   .skip 4096
.balign 16
.skip 16384
boot_stack_top:
.popsection

.pushsection .boot, "ax"
.code32
.global _start
_start:
    cli
    movl $boot_stack_top, %esp
    # ebx = Multiboot info pointer, eax = loader magic (saved in esi); keep
    # both untouched until long_mode_entry.
    movl %eax, %esi

    movl $boot_pdpt, %eax
    orl  ${table_entry}, %eax
    movl %eax, {pml4}
    movl $boot_pd, %eax
    orl  ${table_entry}, %eax
    movl %eax, boot_pdpt

    # Identity-map the first 1 GiB with 2 MiB pages.
    xorl %ecx, %ecx
.Lboot_fill_pd:
    movl %ecx, %eax
    shll ${huge_shift}, %eax
    orl  ${huge_entry}, %eax
    movl %eax, boot_pd(,%ecx,8)
    movl $0, boot_pd+4(,%ecx,8)
    incl %ecx
    cmpl ${table_entries}, %ecx
    jne  .Lboot_fill_pd

    movl ${pml4}, %eax
    movl %eax, %cr3

    movl %cr4, %eax
    orl  ${cr4_pae}, %eax
    movl %eax, %cr4

    movl ${msr_efer}, %ecx
    rdmsr
    orl  ${efer_lme_nxe}, %eax
    wrmsr

    movl %cr0, %eax
    orl  ${cr0_pg_pe}, %eax
    movl %eax, %cr0

    lgdt boot_gdt_ptr
    ljmp ${kernel_cs}, $long_mode_entry

.balign 8
boot_gdt:
    .quad 0
    .quad {boot_code}
    .quad {boot_data}
boot_gdt_ptr:
    .word boot_gdt_ptr - boot_gdt - 1
    .long boot_gdt

.code64
long_mode_entry:
    movw ${kernel_ds}, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    xorl %eax, %eax
    movw %ax, %fs
    movw %ax, %gs
    movl $boot_stack_top, %esp
    movl %ebx, %edi                # arg0 = Multiboot info pointer
                                   # arg1 = magic, already in esi
    xorl %ebp, %ebp
    call {entry}
.Lboot_hang:
    cli
    hlt
    jmp .Lboot_hang
.popsection
"#,
    pml4 = sym BOOT_PML4,
    entry = sym boot_entry,
    mb_magic = const Magic::Header as u32,
    mb_flags = const HEADER_FLAGS,
    mb_checksum = const HEADER_CHECKSUM,
    table_entry = const TABLE_ENTRY,
    huge_entry = const HUGE_PAGE_ENTRY,
    huge_shift = const HUGE_PAGE_SHIFT,
    table_entries = const TABLE_ENTRIES,
    cr4_pae = const Cr4::bits(&[Cr4::PhysicalAddressExtension]),
    msr_efer = const Msr::Efer as u32,
    efer_lme_nxe = const Efer::bits(&[Efer::LongMode, Efer::NoExecute]),
    cr0_pg_pe = const Cr0::bits(&[Cr0::Paging, Cr0::ProtectionEnable]),
    kernel_cs = const Selector::KernelCode as u16,
    kernel_ds = const Selector::KernelData as u16,
    boot_code = const BOOT_CODE_SEGMENT,
    boot_data = const BOOT_DATA_SEGMENT,
    options(att_syntax)
);
