ENTRY(_start)
SECTIONS {
    . = 1M;
    .text   : { KEEP(*(.multiboot)) *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    .data   : { *(.data .data.*) }
    .bss    : { __bss_start = .; *(.bss .bss.*) *(COMMON) . = ALIGN(8); __bss_end = .; }
    _end = .;
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
