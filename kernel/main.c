/* Hardware bring-up: serial console, IDT, TSS, physical memory, kmain(). */
#include "kernel.h"
#include <stdarg.h>

u64 cur_kstack_top;

static inline void outb(u16 p, u8 v)
{
	__asm__ volatile("outb %0,%1" ::"a"(v), "Nd"(p));
}
static inline u8 inb(u16 p)
{
	u8 v;
	__asm__ volatile("inb %1,%0" : "=a"(v) : "Nd"(p));
	return v;
}

/* ---- libc-ish helpers ---- */
void *memset(void *d, int c, u64 n)
{
	void *r = d;
	__asm__ volatile("rep stosb" : "+D"(d), "+c"(n) : "a"(c) : "memory");
	return r;
}
void *memcpy(void *d, const void *s, u64 n)
{
	void *r = d;
	__asm__ volatile("rep movsb" : "+D"(d), "+S"(s), "+c"(n)::"memory");
	return r;
}
u64 strlen(const char *s)
{
	u64 n = 0;
	while (s[n])
		n++;
	return n;
}
int strcmp(const char *a, const char *b)
{
	while (*a && *a == *b) {
		a++;
		b++;
	}
	return (u8)*a - (u8)*b;
}
int strncmp(const char *a, const char *b, u64 n)
{
	while (n && *a && *a == *b) {
		a++;
		b++;
		n--;
	}
	return n ? (u8)*a - (u8)*b : 0;
}

/* ---- serial console (COM1, polled) ---- */
#define COM1 0x3F8
void serial_init(void)
{
	outb(COM1 + 1, 0x00);
	outb(COM1 + 3, 0x80);
	outb(COM1 + 0, 0x01);
	outb(COM1 + 1, 0x00);
	outb(COM1 + 3, 0x03);
	outb(COM1 + 2, 0xC7);
	outb(COM1 + 4, 0x0B);
}
void serial_putc(char c)
{
	if (c == '\n')
		serial_putc('\r');
	while (!(inb(COM1 + 5) & 0x20))
		;
	outb(COM1, (u8)c);
}
int serial_getc_poll(void)
{
	return (inb(COM1 + 5) & 1) ? inb(COM1) : -1;
}
void kputs(const char *s)
{
	while (*s)
		serial_putc(*s++);
}

static void put_num(u64 v, int base, int neg)
{
	char b[24];
	int i = 0;
	if (!v)
		b[i++] = '0';
	while (v) {
		b[i++] = "0123456789abcdef"[v % base];
		v /= base;
	}
	if (neg)
		serial_putc('-');
	while (i)
		serial_putc(b[--i]);
}
void kprintf(const char *f, ...)
{
	va_list ap;
	va_start(ap, f);
	for (; *f; f++) {
		if (*f != '%') {
			serial_putc(*f);
			continue;
		}
		int l = 0;
		f++;
		if (*f == 'l') {
			l = 1;
			f++;
		}
		switch (*f) {
		case 'd': {
			i64 v = l ? va_arg(ap, long) : va_arg(ap, int);
			put_num(v < 0 ? -v : v, 10, v < 0);
			break;
		}
		case 'u': {
			u64 v = l ? va_arg(ap, u64) : va_arg(ap, u32);
			put_num(v, 10, 0);
			break;
		}
		case 'x': {
			u64 v = l ? va_arg(ap, u64) : va_arg(ap, u32);
			put_num(v, 16, 0);
			break;
		}
		case 's': {
			const char *s = va_arg(ap, const char *);
			kputs(s ? s : "(null)");
			break;
		}
		case 'c':
			serial_putc((char)va_arg(ap, int));
			break;
		case '%':
			serial_putc('%');
			break;
		default:
			serial_putc('?');
			break;
		}
	}
	va_end(ap);
}
void panic(const char *msg)
{
	kprintf("\nKERNEL PANIC: %s\n", msg);
	for (;;)
		__asm__ volatile("cli; hlt");
}

/* ---- physical page allocator (identity-mapped, free list + bump) ---- */
extern char _end[];
static u64 free_head, mem_next, mem_end;
static void mm_init(u32 mbi_addr)
{
	u64 top = 0x4000000; /* assume 64MB if no info */
	if (mbi_addr) {
		u32 *mbi = (u32 *)(u64)mbi_addr;
		if (mbi[0] & 1)
			top = 0x100000UL + (u64)mbi[2] * 1024;
	}
	if (top > 0x40000000UL)
		top = 0x40000000UL; /* we only identity-map 1GB */
	mem_next = ((u64)_end + 0x1FFFFF) &
		   ~0x1FFFFFUL; /* skip past kernel + boot-loader scraps */
	mem_end = top;
	kprintf("mm: %lu KB free for pages\n", (mem_end - mem_next) / 1024);
}
void *palloc(void)
{
	u64 p;
	if (free_head) {
		p = free_head;
		free_head = *(u64 *)p;
	} else if (mem_next + PAGE <= mem_end) {
		p = mem_next;
		mem_next += PAGE;
	} else
		return NULL;
	memset((void *)p, 0, PAGE);
	return (void *)p;
}
void pfree(void *p)
{
	*(u64 *)p = free_head;
	free_head = (u64)p;
}

/* ---- TSS ---- */
struct tss {
	u32 r0;
	u64 rsp0, rsp1, rsp2, r1, ist[7], r2;
	u16 r3, iopb;
} __attribute__((packed));
static struct tss tss;
extern u64 gdt[7];
static void tss_init(void)
{
	u64 base = (u64)&tss, lim = sizeof tss - 1;
	gdt[5] = (lim & 0xFFFF) | ((base & 0xFFFFFF) << 16) | (0x89UL << 40) |
		 (((lim >> 16) & 0xF) << 48) | (((base >> 24) & 0xFF) << 56);
	gdt[6] = base >> 32;
	tss.iopb = sizeof tss;
	__asm__ volatile("ltr %w0" ::"r"((u16)0x28));
}
void tss_set_rsp0(u64 v)
{
	tss.rsp0 = v;
	cur_kstack_top = v;
}

/* ---- IDT: CPU exceptions only (no hardware IRQs; scheduling is cooperative) ---- */
struct idt_entry {
	u16 lo, sel;
	u8 ist, attr;
	u16 mid;
	u32 hi, zero;
} __attribute__((packed));
static struct idt_entry idt[32];
extern u64 exc_table[32];
static void idt_init(void)
{
	for (int i = 0; i < 32; i++) {
		u64 a = exc_table[i];
		idt[i] = (struct idt_entry){
			a & 0xFFFF,	    0x08,    0, 0x8E,
			(a >> 16) & 0xFFFF, a >> 32, 0
		};
	}
	struct {
		u16 lim;
		u64 base;
	} __attribute__((packed)) p = { sizeof idt - 1, (u64)idt };
	__asm__ volatile("lidt %0" ::"m"(p));
}
void exception_handler(struct trapframe *tf)
{
	u64 cr2;
	__asm__ volatile("mov %%cr2,%0" : "=r"(cr2));
	int user = (tf->cs & 3) == 3;
	kprintf("\n[exception %lu err=%lx rip=%lx cr2=%lx %s]\n", tf->vec,
		tf->err, tf->rip, cr2, user ? "user" : "kernel");
	if (user && cur) {
		int sig = tf->vec == 0 ? 8 :
			  tf->vec == 6 ? 4 :
					 11; /* SIGFPE / SIGILL / SIGSEGV */
		kprintf("pid %d killed by signal %d\n", cur->pid, sig);
		proc_exit(sig);
	}
	panic("unhandled kernel exception");
}

void kmain(u32 mbi)
{
	serial_init();
	kprintf("HaisuliOS x86_64 booting\n");
	idt_init();
	tss_init();
	mm_init(mbi);
	fs_init();
	syscall_init();
	proc_start_init();
}
