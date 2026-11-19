// SPDX-License-Identifier: MIT

//! Boot entry.
//!
//! The kernel is an arm64 Linux *Image*: a flat binary whose first 64 bytes are the header that
//! boot loaders (and QEMU's `-kernel`) look for. The loader puts it at [`TEXT_OFFSET`] above a
//! 2 MiB boundary near the start of RAM (QEMU's `virt`: physical `0x4008_0000`), and enters it at
//! EL1 with the MMU off, the device tree's physical address in `x0`.
//!
//! Until the MMU is on, everything is assembly: the image is linked at its virtual address in
//! the upper half ([`super::KERNEL_OFFSET`] + physical), and Rust code would find absolute
//! addresses that do not work yet. The boot code zeroes the bss, builds the boot tables, turns on
//! the MMU and jumps to the upper half, then calls [`boot_entry`], which calls [`crate::kmain`].
//!
//! One set of boot tables serves both halves of the address space: `L0[0]` points at an `L1`
//! table with two 1 GiB blocks, the devices from 0 (uncached) and the RAM from 1 GiB. Seen from
//! `TTBR1_EL1`, that is the kernel's linear map of physical memory, its image included, for
//! good. Seen from `TTBR0_EL1`, it is the identity map that the instructions right after turning
//! on the MMU need; each CPU replaces it with the empty root as soon as it runs in the upper half
//! (see [`super::paging`]).
//!
//! The other CPUs start at [`secondary_entry`] (PSCI `CPU_ON` passes their index in `x0`), turn
//! on the MMU with the same tables, and call [`super::smp`]'s entry on a stack of their own.

use super::paging::{desc, mair, KERNEL_DEVICE_BLOCK, KERNEL_NORMAL_BLOCK};
use super::KERNEL_OFFSET;
use core::arch::global_asm;
use core::sync::atomic::AtomicU64;

/// Where the loader puts the image above a 2 MiB boundary (the Image header's `text_offset`);
/// the linker script links it there.
pub(crate) const TEXT_OFFSET: u64 = 0x8_0000;
/// The Image header's flags: little-endian, 4 KiB pages, near the start of RAM.
const IMAGE_FLAGS: u64 = 0b010;
/// Bytes of the boot CPU's stack, on which it later idles.
const BOOT_STACK_SIZE: usize = 16 * 1024;

/// TCR_EL1: 48-bit virtual addresses in both halves (T0SZ = T1SZ = 16), 4 KiB granules, table
/// walks inner shareable and write-back cacheable, 40-bit physical addresses.
const TCR: u64 = 16 // T0SZ
    | 0b01 << 8 // IRGN0: write-back, write-allocate
    | 0b01 << 10 // ORGN0
    | 0b11 << 12 // SH0: inner shareable (TG0 = 0: 4 KiB)
    | 16 << 16 // T1SZ
    | 0b01 << 24 // IRGN1
    | 0b01 << 26 // ORGN1
    | 0b11 << 28 // SH1
    | 0b10 << 30 // TG1: 4 KiB
    | 0b010 << 32; // IPS: 40 bits

/// SCTLR_EL1: the MMU (M), data and instruction caches (C, I) and the stack alignment check at
/// EL1 (SA); EL0 may use `wfi`/`wfe` and read the cache type (nTWI, nTWE, UCT), zero cache lines
/// (DZE) and clean them (UCI); and the bits that are reserved as 1.
const SCTLR: u64 = 1 << 0 // M
    | 1 << 2 // C
    | 1 << 3 // SA
    | 1 << 12 // I
    | 1 << 14 // DZE
    | 1 << 15 // UCT
    | 1 << 16 // nTWI
    | 1 << 18 // nTWE
    | 1 << 26 // UCI
    | 1 << 11 | 1 << 20 | 1 << 22 | 1 << 23 | 1 << 28 | 1 << 29; // RES1

/// CPACR_EL1: FP and SIMD instructions do not trap, at EL0 or EL1 (FPEN). The kernel's own code
/// uses the SIMD registers (see [`super::entry`]).
const CPACR_FP: u64 = 0b11 << 20;

#[repr(C, align(4096))]
pub(crate) struct Table(pub(crate) [AtomicU64; 512]);

/// The boot tables' root and their one `L1` table (see the module documentation).
static BOOT_L0: Table = Table([const { AtomicU64::new(0) }; 512]);
static BOOT_L1: Table = Table([const { AtomicU64::new(0) }; 512]);

/// The boot CPU's stack (in the bss, which is zeroed before the stack is in use).
#[repr(C, align(16))]
struct Stack([AtomicU64; BOOT_STACK_SIZE / 8]);
static BOOT_STACK: Stack = Stack([const { AtomicU64::new(0) }; BOOT_STACK_SIZE / 8]);

extern "C" {
    /// Where the other CPUs start (physical address; see the module documentation).
    pub(crate) fn secondary_entry();
}

/// The physical address of [`secondary_entry`], for PSCI `CPU_ON`.
pub(crate) fn secondary_entry_phys() -> u64 {
    secondary_entry as *const () as u64 - KERNEL_OFFSET
}

global_asm!(
    r#"
.pushsection .head, "ax"
.global _start
_start:
    b primary_entry
    .long 0
    .quad {text_offset}
    .quad __kernel_size
    .quad {image_flags}
    .quad 0
    .quad 0
    .quad 0
    .ascii "ARM\x64"
    .long 0

primary_entry:
    mov x19, x0                         // the device tree
    mrs x1, CurrentEL
    cmp x1, #(1 << 2)                   // EL1?
    b.ne park

    // Zero the bss, at physical addresses (PC-relative `adrp` gives them while the MMU is off).
    adrp x1, __bss_start
    add x1, x1, :lo12:__bss_start
    adrp x2, __bss_end
    add x2, x2, :lo12:__bss_end
1:  cmp x1, x2
    b.hs 2f
    str xzr, [x1], #8
    b 1b
2:
    // The boot tables: L0[0] -> L1; L1[0]: devices, L1[1]: RAM.
    adrp x1, {l0}
    adrp x2, {l1}
    orr x3, x2, #{table}
    str x3, [x1]
    ldr x3, ={device_block}
    str x3, [x2]
    ldr x3, ={ram_block}
    str x3, [x2, #8]
    dsb sy

    bl mmu_on
    ldr x1, =primary_high
    br x1
primary_high:
    adrp x1, {boot_stack}
    add x1, x1, :lo12:{boot_stack}
    add x1, x1, #{boot_stack_size}
    mov sp, x1
    mov x29, xzr
    mov x30, xzr
    mov x0, x19
    bl {boot_entry}
park:
    wfi
    b park

.global secondary_entry
secondary_entry:
    mov x19, x0                         // the CPU's index
    bl mmu_on
    ldr x1, =secondary_high
    br x1
secondary_high:
    adrp x1, {ap_stacks}
    add x1, x1, :lo12:{ap_stacks}
    ldr x2, ={ap_stack_size}
    madd x1, x19, x2, x1
    add x1, x1, x2                      // the top of stack `index`
    mov sp, x1
    mov x29, xzr
    mov x30, xzr
    mov x0, x19
    bl {ap_start}
    b park

// Turns on the MMU with the boot tables in both halves, FP/SIMD and the exception vectors; called
// with the MMU off, and returns to the (identity-mapped) caller.
mmu_on:
    ldr x1, ={mair}
    msr mair_el1, x1
    ldr x1, ={tcr}
    msr tcr_el1, x1
    adrp x1, {l0}
    msr ttbr0_el1, x1
    msr ttbr1_el1, x1
    ldr x1, ={cpacr}
    msr cpacr_el1, x1
    isb
    tlbi vmalle1
    dsb nsh
    isb
    ldr x1, ={sctlr}
    msr sctlr_el1, x1
    isb
    ldr x1, =exception_vectors
    msr vbar_el1, x1
    isb
    ret
.ltorg
.popsection
"#,
    text_offset = const TEXT_OFFSET,
    image_flags = const IMAGE_FLAGS,
    l0 = sym BOOT_L0,
    l1 = sym BOOT_L1,
    table = const desc::VALID | desc::TABLE_OR_PAGE,
    device_block = const KERNEL_DEVICE_BLOCK,
    ram_block = const (1 << 30) | KERNEL_NORMAL_BLOCK,
    boot_stack = sym BOOT_STACK,
    boot_stack_size = const BOOT_STACK_SIZE,
    boot_entry = sym boot_entry,
    ap_stacks = sym super::smp::STACKS,
    ap_stack_size = const super::smp::STACK_SIZE,
    ap_start = sym super::smp::ap_start,
    mair = const mair::MAIR,
    tcr = const TCR,
    cpacr = const CPACR_FP,
    sctlr = const SCTLR,
);

/// First Rust code, on the boot stack in the upper half, with the device tree's physical address.
extern "C" fn boot_entry(device_tree: u64) -> ! {
    super::percpu::init(0); // Before anything that takes a lock.
    crate::platform::qemu_virt::boot(device_tree);
    crate::kmain()
}
