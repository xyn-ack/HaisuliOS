#ifndef KERNEL_H
#define KERNEL_H
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long u64;
typedef long i64;
#define NULL ((void *)0)

#define PAGE 4096UL
#define USER_BASE 0x40000000UL /* user space: 1GB .. 2GB      */
#define USER_TOP 0x80000000UL
#define USER_STACK_PAGES 16
#define NPROC 32
#define NFD 16
#define NFILE 128
#define MAXARG 16
#define PTE_ADDR(e) ((e) & 0x000FFFFFFFFFF000UL)

/* errno values (Linux numbering) */
#define EPERM 1
#define ENOENT 2
#define E2BIG 7
#define ENOEXEC 8
#define EBADF 9
#define ECHILD 10
#define EAGAIN 11
#define ENOMEM 12
#define EACCES 13
#define EFAULT 14
#define ENOTDIR 20
#define EISDIR 21
#define EINVAL 22
#define ENFILE 23
#define EMFILE 24
#define ESPIPE 29
#define EROFS 30
#define ENAMETOOLONG 36
#define ENOSYS 38

/* Layout must match the push order in entry.S (syscall path) */
struct regs {
	u64 r15, r14, r13, r12, rbp, rbx, r10, r9, r8, rdi, rsi, rdx, rax, rcx,
		r11, rsp;
};
/* Layout must match exc_common in entry.S */
struct trapframe {
	u64 r15, r14, r13, r12, r11, r10, r9, r8, rbp, rdi, rsi, rdx, rcx, rbx,
		rax;
	u64 vec, err, rip, cs, rflags, rsp, ss;
};

enum { FILE_NONE, FILE_CONSOLE, FILE_RAM, FILE_DIR };
struct tent {
	char name[104];
	const u8 *data;
	u64 size;
	int isdir;
};
struct file {
	int type, refs;
	const struct tent *ent;
	u64 off;
	char prefix[104];
};

enum { P_UNUSED, P_RUNNABLE, P_ZOMBIE };
struct proc {
	int pid, ppid, state, status;
	u64 *pml4;
	u64 kstack, ksp;
	u64 brk_start, brk, brk_map;
	struct file *fd[NFD];
};
extern struct proc *cur;
extern u64 cur_kstack_top;

/* main.c */
void serial_init(void);
void serial_putc(char c);
int serial_getc_poll(void);
void kputs(const char *s);
void kprintf(const char *fmt, ...);
void panic(const char *msg) __attribute__((noreturn));
void *memset(void *d, int c, u64 n);
void *memcpy(void *d, const void *s, u64 n);
u64 strlen(const char *s);
int strcmp(const char *a, const char *b);
int strncmp(const char *a, const char *b, u64 n);
void *palloc(void);
void pfree(void *p);
void tss_set_rsp0(u64 v);

/* proc.c */
void proc_start_init(void) __attribute__((noreturn));
void proc_exit(int status) __attribute__((noreturn));
void yield(void);
long do_fork(struct regs *r);
long do_wait(int pid, u64 ustatus, int options);
long sys_execve(struct regs *r);
long sys_brk(u64 addr);
int user_ok(u64 va, u64 len);
long ustrcpy(char *dst, u64 src, u64 max);

/* syscall.c */
void fs_init(void);
void syscall_init(void);
const struct tent *fs_lookup(const char *path);
struct file *file_alloc(void);
void file_put(struct file *f);

static inline void write_cr3(u64 v)
{
	__asm__ volatile("mov %0,%%cr3" ::"r"(v) : "memory");
}
#endif
