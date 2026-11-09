// SPDX-License-Identifier: GPL-2.0

//! Multiboot entry.
//!
//! Switches from the loader's 32-bit protected mode to 64-bit long mode and enters Rust.
//!
//! The assembly builds an identity map of the first 1 GiB (2 MiB pages), enables PAE, long mode and
//! NX, loads a temporary GDT and calls `boot_entry`, which calls [`crate::kmain`].
//!
//! These page tables stay in use as the *kernel half* of every address space: PML4[0] is shared by
//! all processes, and user space lives in PML4[1..256]. The top-level table is a Rust static so the
//! rest of the kernel can read it without unsafe code; the assembly refers to it (and to the Rust
//! entry point) with `sym` operands.

use super::multiboot;
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
    if magic == multiboot::BOOTLOADER_MAGIC {
        // SAFETY: A Multiboot loader passed `mb_info` in ebx, and the boot
        // code above has not touched memory outside the kernel image.
        unsafe { multiboot::init(mb_info as u64) };
    }
    crate::kmain()
}

global_asm!(
    r#"
.pushsection .multiboot, "a"
.balign 4
.long 0x1BADB002          # magic
.long 0x00000003          # flags: page-align modules, want memory info/map
.long 0xE4524FFB          # checksum = -(magic + flags)
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
    orl  $0x3, %eax
    movl %eax, {pml4}
    movl $boot_pd, %eax
    orl  $0x3, %eax
    movl %eax, boot_pdpt

    xorl %ecx, %ecx
.Lboot_fill_pd:
    movl %ecx, %eax
    shll $21, %eax
    orl  $0x83, %eax               # present | writable | 2 MiB page
    movl %eax, boot_pd(,%ecx,8)
    movl $0, boot_pd+4(,%ecx,8)
    incl %ecx
    cmpl $512, %ecx
    jne  .Lboot_fill_pd

    movl ${pml4}, %eax
    movl %eax, %cr3

    movl %cr4, %eax
    orl  $0x20, %eax               # CR4.PAE
    movl %eax, %cr4

    movl $0xC0000080, %ecx         # IA32_EFER
    rdmsr
    orl  $0x900, %eax              # LME (bit 8) | NXE (bit 11)
    wrmsr

    movl %cr0, %eax
    orl  $0x80000001, %eax         # PG | PE
    movl %eax, %cr0

    lgdt boot_gdt_ptr
    ljmp $0x08, $long_mode_entry

.balign 8
boot_gdt:
    .quad 0
    .quad 0x00209A0000000000       # 64-bit code
    .quad 0x0000920000000000       # data
boot_gdt_ptr:
    .word boot_gdt_ptr - boot_gdt - 1
    .long boot_gdt

.code64
long_mode_entry:
    movw $0x10, %ax
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
    options(att_syntax)
);
