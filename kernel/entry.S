.code64
.section .text
.globl syscall_entry, fork_ret, switch_to, exc_table
.extern syscall_dispatch, exception_handler, cur_kstack_top

/* SYSCALL: rax=nr rdi rsi rdx r10 r8 r9 = args, rcx=user rip, r11=user rflags */
syscall_entry:
    mov %rsp, user_rsp_tmp(%rip)
    mov cur_kstack_top(%rip), %rsp
    push user_rsp_tmp(%rip)
    push %r11
    push %rcx
    push %rax
    push %rdx
    push %rsi
    push %rdi
    push %r8
    push %r9
    push %r10
    push %rbx
    push %rbp
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, %rdi
    cld
    call syscall_dispatch
fork_ret:                            /* new processes "return" here too */
    pop %r15
    pop %r14
    pop %r13
    pop %r12
    pop %rbp
    pop %rbx
    pop %r10
    pop %r9
    pop %r8
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rax
    pop %rcx
    pop %r11
    pop %rsp
    sysretq

/* void switch_to(u64 *old_sp, u64 new_sp) */
switch_to:
    push %rbp
    push %rbx
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, (%rdi)
    mov %rsi, %rsp
    pop %r15
    pop %r14
    pop %r13
    pop %r12
    pop %rbx
    pop %rbp
    ret

.macro EXC_NOERR n
exc\n:
    pushq $0
    pushq $\n
    jmp exc_common
.endm
.macro EXC_ERR n
exc\n:
    pushq $\n
    jmp exc_common
.endm
.irp n,0,1,2,3,4,5,6,7,9,15,16,18,19,20,21,22,23,24,25,26,27,28,29,31
    EXC_NOERR \n
.endr
.irp n,8,10,11,12,13,14,17,30
    EXC_ERR \n
.endr

exc_common:
    push %rax
    push %rbx
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %rbp
    push %r8
    push %r9
    push %r10
    push %r11
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, %rdi
    cld
    call exception_handler
1:  cli
    hlt
    jmp 1b

.section .rodata
.balign 8
exc_table:
.irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    .quad exc\n
.endr

.section .bss
.balign 8
user_rsp_tmp: .skip 8
