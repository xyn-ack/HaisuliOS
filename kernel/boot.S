/* Multiboot1 entry: 32-bit protected mode -> 64-bit long mode -> kmain() */
.set MB_MAGIC, 0x1BADB002
.set MB_FLAGS, 0x00000003            /* page-align modules, want memory info */
.section .multiboot, "a"
.balign 4
.long MB_MAGIC, MB_FLAGS, -(MB_MAGIC + MB_FLAGS)

.section .text
.code32
.globl _start
_start:
    cli
    cld
    mov %ebx, %esi                   /* keep multiboot info pointer */
    mov $__bss_start, %edi           /* zero .bss (page tables + boot stack live there) */
    mov $__bss_end, %ecx
    sub %edi, %ecx
    xor %eax, %eax
    rep stosb
    mov $boot_stack_top, %esp

    /* identity-map the first 1GB with 2MB supervisor pages */
    xor %ecx, %ecx
1:  mov %ecx, %eax
    shl $21, %eax
    or  $0x83, %eax                  /* present | writable | PS */
    mov %eax, boot_pd(,%ecx,8)
    inc %ecx
    cmp $512, %ecx
    jne 1b
    mov $boot_pd, %eax
    or  $3, %eax
    mov %eax, boot_pdpt
    mov $boot_pdpt, %eax
    or  $3, %eax
    mov %eax, boot_pml4

    mov $boot_pml4, %eax
    mov %eax, %cr3
    mov %cr4, %eax
    or  $0x20, %eax                  /* PAE */
    mov %eax, %cr4
    mov $0xC0000080, %ecx            /* EFER.LME */
    rdmsr
    or  $0x100, %eax
    wrmsr
    mov %cr0, %eax
    or  $0x80000000, %eax            /* PG */
    mov %eax, %cr0
    lgdt gdt_ptr
    ljmp $0x08, $long_mode

.code64
long_mode:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    xor %eax, %eax
    mov %ax, %fs
    mov %ax, %gs
    mov $boot_stack_top, %rsp
    xor %ebp, %ebp
    mov %esi, %edi                   /* arg0 = multiboot info */
    call kmain
2:  cli
    hlt
    jmp 2b

.section .data
.balign 8
.globl gdt
gdt:
    .quad 0
    .quad 0x00209A0000000000         /* 0x08 kernel code */
    .quad 0x0000920000000000         /* 0x10 kernel data */
    .quad 0x0000F20000000000         /* 0x18 user data   */
    .quad 0x0020FA0000000000         /* 0x20 user code   */
    .quad 0, 0                       /* 0x28 TSS (filled in by tss_init) */
gdt_ptr:
    .word 7*8 - 1
    .long gdt

.section .bss
.balign 4096
.globl boot_pd
boot_pml4: .skip 4096
boot_pdpt: .skip 4096
boot_pd:   .skip 4096
.balign 16
.skip 16384
boot_stack_top:
