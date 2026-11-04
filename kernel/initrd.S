.section .rodata
.balign 8
.globl initrd_start, initrd_end
initrd_start:
    .incbin "initrd.tar"
initrd_end:
