/* SPDX-License-Identifier: MIT */
/*
 * HaisuliOS kernel image for arm64: a Linux arm64 Image (a flat binary made
 * with objcopy), which the boot loader puts at text_offset (0x80000) above the
 * start of RAM: physical 0x40080000 on QEMU's virt. It is linked at its
 * address in the kernel's half, the linear map of physical memory at
 * 0xFFFF000000000000 (see kernel/arch/aarch64/boot.rs).
 */
ENTRY(_start)

SECTIONS
{
	. = 0xFFFF000040080000;
	__kernel_start = .;

	.head   : { KEEP(*(.head)) }

	.text   : ALIGN(4K) { *(.text .text.*) }
	.rodata : ALIGN(4K) { *(.rodata .rodata.*) }

	.data   : ALIGN(4K) { *(.data .data.*) }
	.bss (NOLOAD) : ALIGN(4K) {
		__bss_start = .;
		*(.bss .bss.*) *(COMMON)
		. = ALIGN(16);
		__bss_end = .;
	}

	. = ALIGN(4K);
	__kernel_end = .;
	/* The Image header's image_size: the whole image, its bss included. */
	__kernel_size = __kernel_end - __kernel_start;

	/DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
