/* Processes: address spaces, ELF loading, fork/exec/exit/wait, cooperative scheduler. */
#include "kernel.h"

extern u64 boot_pd[512];
extern char fork_ret[];
extern void switch_to(u64 *old_sp, u64 new_sp);

struct proc procs[NPROC];
struct proc *cur;
static int next_pid = 1;

/* ---------- address spaces ----------
 * PML4[0] -> PDPT; PDPT[0] -> shared kernel PD (0..1GB identity, supervisor only)
 *                    PDPT[1] -> per-process PD  (1GB..2GB, user pages)          */
static u64 *pml4_new(void)
{
	u64 *pml4 = palloc(), *pdpt = palloc(), *pd = palloc();
	if (!pml4 || !pdpt || !pd) {
		if (pml4)
			pfree(pml4);
		if (pdpt)
			pfree(pdpt);
		if (pd)
			pfree(pd);
		return NULL;
	}
	pml4[0] = (u64)pdpt | 7;
	pdpt[0] = (u64)boot_pd | 3;
	pdpt[1] = (u64)pd | 7;
	return pml4;
}
static u64 *pt_walk(u64 *pml4, u64 va, int alloc)
{
	u64 *pdpt = (u64 *)PTE_ADDR(pml4[0]);
	u64 *pd = (u64 *)PTE_ADDR(pdpt[1]);
	u64 i = (va >> 21) & 511;
	if (!(pd[i] & 1)) {
		if (!alloc)
			return NULL;
		u64 *pt = palloc();
		if (!pt)
			return NULL;
		pd[i] = (u64)pt | 7;
	}
	return &((u64 *)PTE_ADDR(pd[i]))[(va >> 12) & 511];
}
static void *user_map(u64 *pml4, u64 va)
{
	u64 *pte = pt_walk(pml4, va, 1);
	if (!pte)
		return NULL;
	if (!(*pte & 1)) {
		void *p = palloc();
		if (!p)
			return NULL;
		*pte = (u64)p | 7;
	}
	return (void *)PTE_ADDR(*pte);
}
static void free_user(u64 *pml4)
{
	if (!pml4)
		return;
	u64 *pdpt = (u64 *)PTE_ADDR(pml4[0]), *pd = (u64 *)PTE_ADDR(pdpt[1]);
	for (int i = 0; i < 512; i++)
		if (pd[i] & 1) {
			u64 *pt = (u64 *)PTE_ADDR(pd[i]);
			for (int j = 0; j < 512; j++)
				if (pt[j] & 1)
					pfree((void *)PTE_ADDR(pt[j]));
			pfree(pt);
		}
	pfree(pd);
	pfree(pdpt);
	pfree(pml4);
}
static int copy_space(u64 *src, u64 *dst)
{
	u64 *pd = (u64 *)PTE_ADDR(((u64 *)PTE_ADDR(src[0]))[1]);
	for (u64 i = 0; i < 512; i++)
		if (pd[i] & 1) {
			u64 *pt = (u64 *)PTE_ADDR(pd[i]);
			for (u64 j = 0; j < 512; j++)
				if (pt[j] & 1) {
					void *p = user_map(
						dst, USER_BASE + (i << 21) +
							     (j << 12));
					if (!p)
						return -1;
					memcpy(p, (void *)PTE_ADDR(pt[j]),
					       PAGE);
				}
		}
	return 0;
}
/* Write into another address space without switching to it. Pages must be mapped. */
static void copy_to_pml4(u64 *pml4, u64 va, const void *src, u64 n)
{
	const u8 *s = src;
	while (n) {
		u64 off = va & (PAGE - 1), c = PAGE - off;
		if (c > n)
			c = n;
		u64 *pte = pt_walk(pml4, va, 0);
		memcpy((u8 *)PTE_ADDR(*pte) + off, s, c);
		va += c;
		s += c;
		n -= c;
	}
}

/* ---------- user pointer validation ---------- */
int user_ok(u64 va, u64 len)
{
	if (va < USER_BASE || va + len < va || va + len > USER_TOP)
		return 0;
	if (!len)
		return 1;
	for (u64 p = va & ~(PAGE - 1); p < va + len; p += PAGE) {
		u64 *pte = pt_walk(cur->pml4, p, 0);
		if (!pte || (*pte & 7) != 7)
			return 0;
	}
	return 1;
}
long ustrcpy(char *dst, u64 src, u64 max)
{
	for (u64 i = 0; i < max; i++) {
		if (!user_ok(src + i, 1))
			return -EFAULT;
		dst[i] = *(char *)(src + i);
		if (!dst[i])
			return (long)i;
	}
	return -ENAMETOOLONG;
}

/* ---------- ELF64 loader ---------- */
struct ehdr {
	u8 ident[16];
	u16 type, machine;
	u32 version;
	u64 entry, phoff, shoff;
	u32 flags;
	u16 ehsize, phentsize, phnum, shentsize, shnum, shstrndx;
};
struct phdr {
	u32 type, flags;
	u64 offset, vaddr, paddr, filesz, memsz, align;
};

static int load_elf(u64 *pml4, const u8 *img, u64 size, u64 *entry, u64 *brk)
{
	const struct ehdr *eh = (const void *)img;
	if (size < sizeof *eh || img[0] != 0x7f || img[1] != 'E' ||
	    img[2] != 'L' || img[3] != 'F' || img[4] != 2 ||
	    eh->machine != 62 || eh->type != 2)
		return -ENOEXEC;
	if (eh->phoff + (u64)eh->phnum * sizeof(struct phdr) > size)
		return -ENOEXEC;
	u64 top = 0, limit = USER_TOP - (USER_STACK_PAGES + 16) * PAGE;
	for (int i = 0; i < eh->phnum; i++) {
		const struct phdr *ph =
			(const struct phdr *)(img + eh->phoff) + i;
		if (ph->type != 1)
			continue;
		if (ph->vaddr < USER_BASE ||
		    ph->vaddr + ph->memsz < ph->vaddr ||
		    ph->vaddr + ph->memsz > limit || ph->filesz > ph->memsz ||
		    ph->offset + ph->filesz > size)
			return -ENOEXEC;
		for (u64 va = ph->vaddr & ~(PAGE - 1);
		     va < ph->vaddr + ph->memsz; va += PAGE)
			if (!user_map(pml4, va))
				return -ENOMEM;
		copy_to_pml4(pml4, ph->vaddr, img + ph->offset, ph->filesz);
		if (ph->vaddr + ph->memsz > top)
			top = ph->vaddr + ph->memsz;
	}
	*entry = eh->entry;
	*brk = (top + PAGE - 1) & ~(PAGE - 1);
	return 0;
}

/* System V style initial stack: argc, argv[], NULL, envp(NULL) */
static u64 setup_stack(u64 *pml4, int argc, char **argv)
{
	for (u64 va = USER_TOP - USER_STACK_PAGES * PAGE; va < USER_TOP;
	     va += PAGE)
		if (!user_map(pml4, va))
			return 0;
	u64 sp = USER_TOP, ptrs[MAXARG + 1], vec[MAXARG + 3];
	for (int i = argc - 1; i >= 0; i--) {
		u64 n = strlen(argv[i]) + 1;
		sp -= n;
		copy_to_pml4(pml4, sp, argv[i], n);
		ptrs[i] = sp;
	}
	u64 words = argc + 3;
	sp = (sp & ~15UL) - ((words * 8 + 15) & ~15UL);
	vec[0] = argc;
	for (int i = 0; i < argc; i++)
		vec[1 + i] = ptrs[i];
	vec[1 + argc] = 0;
	vec[2 + argc] = 0;
	copy_to_pml4(pml4, sp, vec, words * 8);
	return sp;
}

/* ---------- process table ---------- */
static struct proc *proc_alloc(void)
{
	for (int i = 0; i < NPROC; i++)
		if (procs[i].state == P_UNUSED) {
			struct proc *p = &procs[i];
			memset(p, 0, sizeof *p);
			void *ks = palloc();
			if (!ks)
				return NULL;
			p->kstack = (u64)ks;
			p->pid = next_pid++;
			return p;
		}
	return NULL;
}
static void proc_discard(struct proc *p)
{
	free_user(p->pml4);
	pfree((void *)p->kstack);
	for (int i = 0; i < NFD; i++)
		if (p->fd[i])
			file_put(p->fd[i]);
	memset(p, 0, sizeof *p);
}
/* Build a kernel stack so that switch_to() resumes into fork_ret, which sysrets with *r */
static void kstack_init(struct proc *p, const struct regs *r)
{
	struct regs *fr = (struct regs *)(p->kstack + PAGE - sizeof *fr);
	*fr = *r;
	u64 *sp = (u64 *)fr;
	*--sp = (u64)fork_ret;
	for (int i = 0; i < 6; i++)
		*--sp = 0;
	p->ksp = (u64)sp;
}

/* ---------- scheduler (cooperative round-robin) ---------- */
static void switch_proc(struct proc *next)
{
	struct proc *prev = cur;
	cur = next;
	tss_set_rsp0(next->kstack + PAGE);
	write_cr3((u64)next->pml4);
	switch_to(&prev->ksp, next->ksp);
}
void yield(void)
{
	int start = (int)(cur - procs);
	for (int i = 1; i <= NPROC; i++) {
		struct proc *p = &procs[(start + i) % NPROC];
		if (p != cur && p->state == P_RUNNABLE) {
			switch_proc(p);
			return;
		}
	}
}

void proc_start_init(void)
{
	const struct tent *e = fs_lookup("/bin/init");
	if (!e)
		panic("no /bin/init in initrd");
	struct proc *p = proc_alloc();
	if (!p || !(p->pml4 = pml4_new()))
		panic("cannot allocate init");
	u64 entry, brk;
	char *av[] = { "init", NULL };
	if (load_elf(p->pml4, e->data, e->size, &entry, &brk) < 0)
		panic("bad /bin/init");
	u64 sp = setup_stack(p->pml4, 1, av);
	if (!sp)
		panic("init stack");
	p->brk_start = p->brk = p->brk_map = brk;
	struct file *c = file_alloc();
	c->type = FILE_CONSOLE;
	c->refs = 3;
	p->fd[0] = p->fd[1] = p->fd[2] = c;
	struct regs r;
	memset(&r, 0, sizeof r);
	r.rcx = entry;
	r.rsp = sp;
	r.r11 = 2;
	kstack_init(p, &r);
	p->state = P_RUNNABLE;
	cur = p;
	tss_set_rsp0(p->kstack + PAGE);
	write_cr3((u64)p->pml4);
	u64 dummy;
	kprintf("starting /bin/init (pid %d)\n", p->pid);
	switch_to(&dummy, p->ksp);
	panic("returned from init start");
}

long do_fork(struct regs *r)
{
	struct proc *c = proc_alloc();
	if (!c)
		return -EAGAIN;
	c->pml4 = pml4_new();
	if (!c->pml4 || copy_space(cur->pml4, c->pml4) < 0) {
		proc_discard(c);
		return -ENOMEM;
	}
	c->ppid = cur->pid;
	c->brk_start = cur->brk_start;
	c->brk = cur->brk;
	c->brk_map = cur->brk_map;
	for (int i = 0; i < NFD; i++)
		if ((c->fd[i] = cur->fd[i]))
			c->fd[i]->refs++;
	struct regs cr = *r;
	cr.rax = 0;
	kstack_init(c, &cr);
	c->state = P_RUNNABLE;
	return c->pid;
}

void proc_exit(int status)
{
	for (int i = 0; i < NFD; i++)
		if (cur->fd[i]) {
			file_put(cur->fd[i]);
			cur->fd[i] = NULL;
		}
	for (int i = 0; i < NPROC; i++)
		if (procs[i].state != P_UNUSED && procs[i].ppid == cur->pid)
			procs[i].ppid = 1;
	cur->status = status;
	if (cur->pid == 1) {
		kprintf("\ninit exited (status %x); halting\n", status);
		for (;;)
			__asm__ volatile("cli; hlt");
	}
	cur->state = P_ZOMBIE;
	for (;;)
		yield();
}

long do_wait(int pid, u64 ustatus, int options)
{
	for (;;) {
		int have = 0;
		for (int i = 0; i < NPROC; i++) {
			struct proc *p = &procs[i];
			if (p->state == P_UNUSED || p->ppid != cur->pid)
				continue;
			if (pid > 0 && p->pid != pid)
				continue;
			have = 1;
			if (p->state == P_ZOMBIE) {
				if (ustatus) {
					if (!user_ok(ustatus, 4))
						return -EFAULT;
					*(int *)ustatus = p->status;
				}
				int id = p->pid;
				proc_discard(p);
				return id;
			}
		}
		if (!have)
			return -ECHILD;
		if (options & 1)
			return 0; /* WNOHANG */
		yield();
	}
}

long sys_execve(struct regs *r)
{
	static char path[128], argbuf[2048], *argv[MAXARG + 1];
	long n = ustrcpy(path, r->rdi, sizeof path);
	if (n < 0)
		return n;
	int argc = 0;
	u64 off = 0;
	if (r->rsi)
		for (;;) {
			u64 slot = r->rsi + (u64)argc * 8;
			if (!user_ok(slot, 8))
				return -EFAULT;
			u64 ap = *(u64 *)slot;
			if (!ap)
				break;
			if (argc >= MAXARG)
				return -E2BIG;
			long l = ustrcpy(argbuf + off, ap, sizeof argbuf - off);
			if (l < 0)
				return l == -ENAMETOOLONG ? -E2BIG : l;
			argv[argc++] = argbuf + off;
			off += l + 1;
		}
	argv[argc] = NULL;
	const struct tent *e = fs_lookup(path);
	if (!e)
		return -ENOENT;
	if (e->isdir)
		return -EACCES;
	u64 *np = pml4_new();
	if (!np)
		return -ENOMEM;
	u64 entry, brk;
	int rc = load_elf(np, e->data, e->size, &entry, &brk);
	if (rc < 0) {
		free_user(np);
		return rc;
	}
	u64 sp = setup_stack(np, argc, argv);
	if (!sp) {
		free_user(np);
		return -ENOMEM;
	}
	u64 *old = cur->pml4;
	cur->pml4 = np;
	cur->brk_start = cur->brk = cur->brk_map = brk;
	write_cr3((u64)np);
	free_user(old);
	r->rcx = entry;
	r->rsp = sp;
	r->r11 = 2;
	return 0;
}

long sys_brk(u64 addr)
{
	u64 limit = USER_TOP - (USER_STACK_PAGES + 16) * PAGE;
	if (addr == 0 || addr < cur->brk_start || addr > limit)
		return (long)cur->brk;
	u64 want = (addr + PAGE - 1) & ~(PAGE - 1);
	for (; cur->brk_map < want; cur->brk_map += PAGE)
		if (!user_map(cur->pml4, cur->brk_map))
			return (long)cur->brk;
	cur->brk = addr;
	return (long)addr;
}
