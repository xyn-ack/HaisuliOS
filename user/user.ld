/* SPDX-License-Identifier: GPL-2.0 */
/*
 * HaisuliOS user programs: static executables at USER_BASE (PML4[1]).
 * Every section starts on its own page so the kernel's ELF loader can give
 * each segment exact permissions (R-X text, R-- rodata, RW- data/bss).
 */
ENTRY(_start)

SECTIONS
{
	. = 0x8000000000;

	.text   : ALIGN(4K) { *(.text .text.*) }
	.rodata : ALIGN(4K) { *(.rodata .rodata.*) }
	.data   : ALIGN(4K) { *(.data .data.*) *(.got .got.*) }
	.bss    : ALIGN(4K) { *(.bss .bss.*) *(COMMON) }

	/DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
