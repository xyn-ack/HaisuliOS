#include "ulib.h"
#include <stdarg.h>

static inline long sc(long n, long a, long b, long c, long d)
{
	long r;
	register long r10 __asm__("r10") = d;
	__asm__ volatile("syscall"
			 : "=a"(r)
			 : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10)
			 : "rcx", "r11", "memory");
	return r;
}
int errno;
/* raw syscalls return -errno on failure; convert to the POSIX -1/errno style */
static long ck(long r)
{
	if (r < 0 && r > -4096) {
		errno = (int)-r;
		return -1;
	}
	return r;
}
long write(int fd, const void *b, size_t n)
{
	return ck(sc(1, fd, (long)b, n, 0));
}
long read(int fd, void *b, size_t n)
{
	return ck(sc(0, fd, (long)b, n, 0));
}
int open(const char *p, int fl)
{
	return (int)ck(sc(2, (long)p, fl, 0, 0));
}
int close(int fd)
{
	return (int)ck(sc(3, fd, 0, 0, 0));
}
int pipe(int fds[2])
{
	return (int)ck(sc(22, (long)fds, 0, 0, 0));
}
int dup(int fd)
{
	return (int)ck(sc(32, fd, 0, 0, 0));
}
int dup2(int a, int b)
{
	return (int)ck(sc(33, a, b, 0, 0));
}
int fork(void)
{
	return (int)ck(sc(57, 0, 0, 0, 0));
}
int execve(const char *p, char *const a[], char *const e[])
{
	return (int)ck(sc(59, (long)p, (long)a, (long)e, 0));
}
int waitpid(int pid, int *st, int opt)
{
	return (int)ck(sc(61, pid, (long)st, opt, 0));
}
void exit(int s)
{
	for (;;)
		sc(60, s, 0, 0, 0);
}
int getpid(void)
{
	return (int)sc(39, 0, 0, 0, 0);
}
int getppid(void)
{
	return (int)sc(110, 0, 0, 0, 0);
}
long getdents64(int fd, void *b, size_t n)
{
	return ck(sc(217, fd, (long)b, n, 0));
}
void yield(void)
{
	sc(24, 0, 0, 0, 0);
}
void *sbrk(long inc)
{
	long old = sc(12, 0, 0, 0, 0);
	if (!inc)
		return (void *)old;
	long nw = sc(12, old + inc, 0, 0, 0);
	if (nw != old + inc) {
		errno = ENOMEM;
		return (void *)-1;
	}
	return (void *)old;
}

size_t strlen(const char *s)
{
	size_t n = 0;
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
	return (unsigned char)*a - (unsigned char)*b;
}
char *strchr(const char *s, int c)
{
	for (; *s; s++)
		if (*s == c)
			return (char *)s;
	return c ? 0 : (char *)s;
}
void *memcpy(void *d, const void *s, size_t n)
{
	char *p = d;
	const char *q = s;
	while (n--)
		*p++ = *q++;
	return d;
}
int memcmp(const void *a, const void *b, size_t n)
{
	const unsigned char *p = a, *q = b;
	for (; n; n--, p++, q++)
		if (*p != *q)
			return *p - *q;
	return 0;
}
void *memset(void *d, int c, size_t n)
{
	char *p = d;
	while (n--)
		*p++ = (char)c;
	return d;
}

static char obuf[512];
static int olen, ofd;
static void oflush(void)
{
	if (olen)
		write(ofd, obuf, olen);
	olen = 0;
}
static void oc(char c)
{
	if (olen == sizeof obuf)
		oflush();
	obuf[olen++] = c;
}
static void onum(unsigned long v, int base, int neg)
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
		oc('-');
	while (i)
		oc(b[--i]);
}
/* Output is flushed before returning, so nothing lingers across fork/exec. */
static int vdprintf(int fd, const char *f, va_list ap)
{
	ofd = fd;
	for (; *f; f++) {
		if (*f != '%') {
			oc(*f);
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
			long v = l ? va_arg(ap, long) : va_arg(ap, int);
			onum(v < 0 ? -v : v, 10, v < 0);
			break;
		}
		case 'u': {
			unsigned long v = l ? va_arg(ap, unsigned long) :
					      va_arg(ap, unsigned);
			onum(v, 10, 0);
			break;
		}
		case 'x': {
			unsigned long v = l ? va_arg(ap, unsigned long) :
					      va_arg(ap, unsigned);
			onum(v, 16, 0);
			break;
		}
		case 's': {
			const char *s = va_arg(ap, const char *);
			if (!s)
				s = "(null)";
			while (*s)
				oc(*s++);
			break;
		}
		case 'c':
			oc((char)va_arg(ap, int));
			break;
		case '%':
			oc('%');
			break;
		}
	}
	oflush();
	return 0;
}
int printf(const char *f, ...)
{
	va_list ap;
	va_start(ap, f);
	int r = vdprintf(1, f, ap);
	va_end(ap);
	return r;
}
int dprintf(int fd, const char *f, ...)
{
	va_list ap;
	va_start(ap, f);
	int r = vdprintf(fd, f, ap);
	va_end(ap);
	return r;
}
static const char *const errmsg[] = {
	[EPERM] = "Operation not permitted",
	[ENOENT] = "No such file or directory",
	[E2BIG] = "Argument list too long",
	[ENOEXEC] = "Exec format error",
	[EBADF] = "Bad file descriptor",
	[ECHILD] = "No child processes",
	[EAGAIN] = "Resource temporarily unavailable",
	[ENOMEM] = "Out of memory",
	[EACCES] = "Permission denied",
	[EFAULT] = "Bad address",
	[ENOTDIR] = "Not a directory",
	[EISDIR] = "Is a directory",
	[EINVAL] = "Invalid argument",
	[ENFILE] = "Too many open files in system",
	[EMFILE] = "Too many open files",
	[ESPIPE] = "Illegal seek",
	[EROFS] = "Read-only file system",
	[EPIPE] = "Broken pipe",
	[ENAMETOOLONG] = "File name too long",
	[ENOSYS] = "Function not implemented",
};
char *strerror(int e)
{
	if (e > 0 && e < (int)(sizeof errmsg / sizeof *errmsg) && errmsg[e])
		return (char *)errmsg[e];
	return "Unknown error";
}
void perror(const char *s)
{
	if (s && *s)
		dprintf(2, "%s: %s\n", s, strerror(errno));
	else
		dprintf(2, "%s\n", strerror(errno));
}

extern int main(int, char **);
void _cstart(unsigned long *sp)
{
	exit(main((int)sp[0], (char **)(sp + 1)));
}
__asm__(".text\n.globl _start\n_start:\n xor %ebp,%ebp\n mov %rsp,%rdi\n and $-16,%rsp\n call _cstart\n ud2\n");
