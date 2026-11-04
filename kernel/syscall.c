/* Read-only tar initrd filesystem, file descriptors, tty, and the syscall table.
 * Syscall numbers and errno values follow the Linux x86_64 ABI. */
#include "kernel.h"

extern u8 initrd_start[], initrd_end[];
extern void syscall_entry(void);

/* ---------- initrd (ustar) ---------- */
#define MAXTENT 128
static struct tent tents[MAXTENT];
static int ntent;
static const struct tent root_ent = { "", NULL, 0, 1 };

void fs_init(void)
{
	u8 *p = initrd_start;
	while (p + 512 <= initrd_end && p[0]) {
		u64 size = 0;
		for (int i = 124; i < 136; i++) {
			if (p[i] >= '0' && p[i] <= '7')
				size = size * 8 + (p[i] - '0');
			else if (p[i] != ' ')
				break;
		}
		u8 type = p[156];
		const char *nm = (const char *)p;
		while (nm[0] == '.' && nm[1] == '/')
			nm += 2;
		if ((type == '0' || type == 0 || type == '5') &&
		    ntent < MAXTENT) {
			struct tent *t = &tents[ntent];
			int n = 0;
			while (n < 100 && nm[n] && nm < (const char *)p + 100)
				n++;
			memcpy(t->name, nm, n);
			t->name[n] = 0;
			while (n && t->name[n - 1] == '/')
				t->name[--n] = 0;
			t->data = p + 512;
			t->size = size;
			t->isdir = (type == '5');
			if (n)
				ntent++;
		}
		p += 512 + ((size + 511) & ~511UL);
	}
	kprintf("initrd: %d entries\n", ntent);
}
static void norm(const char *in, char *out)
{
	while (*in == '/' || (in[0] == '.' && in[1] == '/'))
		in += (*in == '/') ? 1 : 2;
	int n = 0;
	while (*in && n < 103)
		out[n++] = *in++;
	while (n && out[n - 1] == '/')
		n--;
	out[n] = 0;
	if (!strcmp(out, "."))
		out[0] = 0;
}
const struct tent *fs_lookup(const char *path)
{
	char p[104];
	norm(path, p);
	if (!p[0])
		return &root_ent;
	for (int i = 0; i < ntent; i++)
		if (!strcmp(tents[i].name, p))
			return &tents[i];
	return NULL;
}
/* first path component of tent name below prefix; returns 0 if not under prefix */
static int comp(const char *nm, const char *prefix, u64 plen, const char **s,
		u64 *len)
{
	if (plen) {
		if (strncmp(nm, prefix, plen) || nm[plen] != '/')
			return 0;
		nm += plen + 1;
	}
	if (!*nm)
		return 0;
	u64 l = 0;
	while (nm[l] && nm[l] != '/')
		l++;
	*s = nm;
	*len = l;
	return 1;
}
static int dir_entry(const char *prefix, int n, char *name, int *isdir)
{
	u64 plen = strlen(prefix);
	int count = 0;
	for (int i = 0; i < ntent; i++) {
		const char *s;
		u64 l;
		if (!comp(tents[i].name, prefix, plen, &s, &l))
			continue;
		int dup = 0;
		for (int j = 0; j < i && !dup; j++) {
			const char *s2;
			u64 l2;
			if (comp(tents[j].name, prefix, plen, &s2, &l2) &&
			    l2 == l && !strncmp(s, s2, l))
				dup = 1;
		}
		if (dup)
			continue;
		if (count++ == n) {
			memcpy(name, s, l);
			name[l] = 0;
			*isdir = s[l] == '/' || tents[i].isdir;
			return 1;
		}
	}
	return 0;
}

/* ---------- open files ---------- */
static struct file files[NFILE];
struct file *file_alloc(void)
{
	for (int i = 0; i < NFILE; i++)
		if (!files[i].refs) {
			memset(&files[i], 0, sizeof files[i]);
			files[i].refs = 1;
			return &files[i];
		}
	return NULL;
}
void file_put(struct file *f)
{
	if (--f->refs)
		return;
	if (f->type == FILE_PIPE_R || f->type == FILE_PIPE_W)
		pipe_close(f->pipe, f->type == FILE_PIPE_W);
	f->type = FILE_NONE;
	f->pipe = NULL;
}
static struct file *getfd(int fd)
{
	return (fd < 0 || fd >= NFD) ? NULL : cur->fd[fd];
}
static int fd_install(struct file *f)
{
	for (int i = 0; i < NFD; i++)
		if (!cur->fd[i]) {
			cur->fd[i] = f;
			return i;
		}
	return -EMFILE;
}

/* ---------- tty: canonical line discipline over COM1 ---------- */
static char linebuf[256];
static int linelen, linepos;
static int tty_getc(void)
{
	int c;
	while ((c = serial_getc_poll()) < 0)
		yield();
	return c;
}
static long tty_read(char *dst, u64 n)
{
	if (linepos >= linelen) {
		linelen = linepos = 0;
		for (;;) {
			int c = tty_getc();
			if (c == '\r')
				c = '\n';
			if (c == 4) {
				if (!linelen)
					return 0;
				break;
			} /* ^D */
			if (c == 127 || c == 8) {
				if (linelen) {
					linelen--;
					kputs("\b \b");
				}
				continue;
			}
			if (c == '\n') {
				linebuf[linelen++] = '\n';
				serial_putc('\n');
				break;
			}
			if (c >= 32 && linelen < (int)sizeof linebuf - 1) {
				linebuf[linelen++] = (char)c;
				serial_putc((char)c);
			}
		}
	}
	u64 c = (u64)(linelen - linepos);
	if (c > n)
		c = n;
	memcpy(dst, linebuf + linepos, c);
	linepos += (int)c;
	return (long)c;
}

/* ---------- syscalls ---------- */
static long sys_read(int fd, u64 buf, u64 n)
{
	struct file *f = getfd(fd);
	if (!f)
		return -EBADF;
	if (!user_ok(buf, n))
		return -EFAULT;
	switch (f->type) {
	case FILE_CONSOLE:
		return tty_read((char *)buf, n);
	case FILE_RAM: {
		u64 left = f->off < f->ent->size ? f->ent->size - f->off : 0,
		    c = n < left ? n : left;
		memcpy((void *)buf, f->ent->data + f->off, c);
		f->off += c;
		return (long)c;
	}
	case FILE_DIR:
		return -EISDIR;
	case FILE_PIPE_R:
		return pipe_read(f->pipe, (char *)buf, n);
	}
	return -EBADF;
}
static long sys_write(int fd, u64 buf, u64 n)
{
	struct file *f = getfd(fd);
	if (!f)
		return -EBADF;
	if (!user_ok(buf, n))
		return -EFAULT;
	if (f->type == FILE_PIPE_W)
		return pipe_write(f->pipe, (const char *)buf, n);
	if (f->type != FILE_CONSOLE)
		return f->type == FILE_DIR ? -EISDIR : -EBADF;
	for (u64 i = 0; i < n; i++)
		serial_putc(((char *)buf)[i]);
	return (long)n;
}
static long sys_open(u64 upath, int flags)
{
	char p[128];
	long n = ustrcpy(p, upath, sizeof p);
	if (n < 0)
		return n;
	struct file *f;
	int fd;
	if (!strcmp(p, "/dev/console") || !strcmp(p, "/dev/tty")) {
		if (!(f = file_alloc()))
			return -ENFILE;
		f->type = FILE_CONSOLE;
	} else {
		if ((flags & 3) || (flags & 0x40))
			return -EROFS; /* read-only filesystem */
		const struct tent *e = fs_lookup(p);
		if (!e)
			return -ENOENT;
		if (!(f = file_alloc()))
			return -ENFILE;
		if (e->isdir) {
			f->type = FILE_DIR;
			memcpy(f->prefix, e->name, strlen(e->name) + 1);
		} else {
			f->type = FILE_RAM;
			f->ent = e;
		}
	}
	if ((fd = fd_install(f)) < 0)
		file_put(f);
	return fd;
}
static long sys_close(int fd)
{
	struct file *f = getfd(fd);
	if (!f)
		return -EBADF;
	cur->fd[fd] = NULL;
	file_put(f);
	return 0;
}
static long sys_lseek(int fd, i64 off, int whence)
{
	struct file *f = getfd(fd);
	if (!f)
		return -EBADF;
	if (f->type == FILE_DIR && whence == 0) {
		f->off = (u64)off;
		return off;
	}
	if (f->type != FILE_RAM)
		return -ESPIPE;
	i64 base = whence == 0 ? 0 :
		   whence == 1 ? (i64)f->off :
		   whence == 2 ? (i64)f->ent->size :
				 -1;
	if (base < 0 || base + off < 0)
		return -EINVAL;
	f->off = (u64)(base + off);
	return base + off;
}
static long sys_dup2(int old, int nw)
{
	struct file *f = getfd(old);
	if (!f)
		return -EBADF;
	if (nw < 0 || nw >= NFD)
		return -EBADF;
	if (old == nw)
		return nw;
	if (cur->fd[nw])
		file_put(cur->fd[nw]);
	cur->fd[nw] = f;
	f->refs++;
	return nw;
}
static long sys_dup(int old)
{
	struct file *f = getfd(old);
	if (!f)
		return -EBADF;
	int fd = fd_install(f);
	if (fd >= 0)
		f->refs++;
	return fd;
}
static long sys_pipe(u64 ufds)
{
	if (!user_ok(ufds, 2 * sizeof(int)))
		return -EFAULT;
	struct pipe *p = pipe_alloc();
	if (!p)
		return -ENFILE;
	/* once both files own the pipe, file_put() handles all cleanup */
	struct file *r = file_alloc();
	if (!r) {
		pipe_close(p, 0);
		pipe_close(p, 1);
		return -ENFILE;
	}
	r->type = FILE_PIPE_R;
	r->pipe = p;
	struct file *w = file_alloc();
	if (!w) {
		file_put(r);
		pipe_close(p, 1);
		return -ENFILE;
	}
	w->type = FILE_PIPE_W;
	w->pipe = p;
	int rfd = fd_install(r), wfd = rfd < 0 ? rfd : fd_install(w);
	if (wfd < 0) {
		if (rfd >= 0)
			cur->fd[rfd] = NULL;
		file_put(r);
		file_put(w);
		return wfd;
	}
	((int *)ufds)[0] = rfd;
	((int *)ufds)[1] = wfd;
	return 0;
}
static long sys_getdents64(int fd, u64 ubuf, u64 count)
{
	struct file *f = getfd(fd);
	if (!f)
		return -EBADF;
	if (f->type != FILE_DIR)
		return -ENOTDIR;
	if (!user_ok(ubuf, count))
		return -EFAULT;
	u64 pos = 0;
	for (;;) {
		char name[104];
		int isdir;
		if (!dir_entry(f->prefix, (int)f->off, name, &isdir))
			break;
		u64 nl = strlen(name), rl = (19 + nl + 1 + 7) & ~7UL;
		if (pos + rl > count) {
			if (!pos)
				return -EINVAL;
			break;
		}
		u8 *d = (u8 *)(ubuf + pos);
		memset(d, 0, rl);
		*(u64 *)d = f->off + 1;
		*(u64 *)(d + 8) = f->off + 1;
		*(u16 *)(d + 16) = (u16)rl;
		d[18] = isdir ? 4 : 8;
		memcpy(d + 19, name, nl);
		pos += rl;
		f->off++;
	}
	return (long)pos;
}

void syscall_dispatch(struct regs *r)
{
	long ret;
	switch (r->rax) {
	case 0:
		ret = sys_read((int)r->rdi, r->rsi, r->rdx);
		break;
	case 1:
		ret = sys_write((int)r->rdi, r->rsi, r->rdx);
		break;
	case 2:
		ret = sys_open(r->rdi, (int)r->rsi);
		break;
	case 3:
		ret = sys_close((int)r->rdi);
		break;
	case 8:
		ret = sys_lseek((int)r->rdi, (i64)r->rsi, (int)r->rdx);
		break;
	case 12:
		ret = sys_brk(r->rdi);
		break;
	case 22:
		ret = sys_pipe(r->rdi);
		break;
	case 24:
		yield();
		ret = 0;
		break;
	case 32:
		ret = sys_dup((int)r->rdi);
		break;
	case 33:
		ret = sys_dup2((int)r->rdi, (int)r->rsi);
		break;
	case 39:
		ret = cur->pid;
		break;
	case 57:
		ret = do_fork(r);
		break;
	case 59:
		ret = sys_execve(r);
		break;
	case 60:
	case 231:
		proc_exit((int)((r->rdi & 0xFF) << 8));
	case 61:
		ret = do_wait((int)r->rdi, r->rsi, (int)r->rdx);
		break;
	case 110:
		ret = cur->ppid;
		break;
	case 217:
		ret = sys_getdents64((int)r->rdi, r->rsi, r->rdx);
		break;
	default:
		ret = -ENOSYS;
		break;
	}
	r->rax = (u64)ret;
}

static inline void wrmsr(u32 m, u64 v)
{
	__asm__ volatile("wrmsr" ::"c"(m), "a"((u32)v), "d"((u32)(v >> 32)));
}
static inline u64 rdmsr(u32 m)
{
	u32 lo, hi;
	__asm__ volatile("rdmsr" : "=a"(lo), "=d"(hi) : "c"(m));
	return ((u64)hi << 32) | lo;
}
void syscall_init(void)
{
	wrmsr(0xC0000080, rdmsr(0xC0000080) | 1); /* EFER.SCE */
	wrmsr(0xC0000081,
	      (0x10UL << 48) |
		      (0x08UL
		       << 32)); /* STAR: sysret base 0x10, syscall CS 0x08 */
	wrmsr(0xC0000082, (u64)syscall_entry); /* LSTAR */
	wrmsr(0xC0000084, 0x600); /* SFMASK: clear IF, DF */
}
