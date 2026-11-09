/* SPDX-License-Identifier: GPL-2.0 */
/*
 * HaisuliOS kernel image: loaded at 1 MiB by a Multiboot (v1) loader and
 * identity mapped.
 */
ENTRY(_start)

SECTIONS
{
	. = 1M;
	__kernel_start = .;

	.multiboot : { KEEP(*(.multiboot)) }
	.boot      : { *(.boot) }

	.text   : ALIGN(4K) { *(.text .text.*) }
	.rodata : ALIGN(4K) { *(.rodata .rodata.*) }

	.data   : ALIGN(4K) { *(.data .data.*) }
	.bss    : ALIGN(4K) { *(COMMON) *(.bss .bss.*) }

	. = ALIGN(4K);
	__kernel_end = .;

	/DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
