/* Anonymous pipes: a one-page ring buffer shared by a read-end and a write-end
 * struct file. Each end is a single struct file (dup/fork only bump its refs),
 * so an end is "open" until the last descriptor referring to it is closed.
 * Blocking is done by yielding, since the scheduler is cooperative. */
#include "kernel.h"

#define PIPE_SIZE PAGE
#define PIPE_BUF 512 /* writes of up to this many bytes are atomic (POSIX) */

struct pipe {
	char *buf; /* NULL when the slot is free */
	u64 rpos, wpos; /* free-running; wpos - rpos bytes are buffered */
	int rd_open, wr_open;
};
static struct pipe pipes[NPIPE];

struct pipe *pipe_alloc(void)
{
	for (int i = 0; i < NPIPE; i++)
		if (!pipes[i].buf) {
			char *b = palloc();
			if (!b)
				return NULL;
			struct pipe *p = &pipes[i];
			p->buf = b;
			p->rpos = p->wpos = 0;
			p->rd_open = p->wr_open = 1;
			return p;
		}
	return NULL;
}

void pipe_close(struct pipe *p, int write_end)
{
	if (write_end)
		p->wr_open = 0;
	else
		p->rd_open = 0;
	if (!p->rd_open && !p->wr_open) {
		pfree(p->buf);
		p->buf = NULL;
	}
}

/* Blocks until at least one byte is buffered; returns 0 (EOF) once the buffer
 * is empty and the write end has been closed. */
long pipe_read(struct pipe *p, char *dst, u64 n)
{
	if (!n)
		return 0;
	while (p->wpos == p->rpos) {
		if (!p->wr_open)
			return 0;
		yield();
	}
	u64 avail = p->wpos - p->rpos, c = n < avail ? n : avail;
	for (u64 done = 0; done < c;) {
		u64 at = (p->rpos + done) % PIPE_SIZE, run = PIPE_SIZE - at;
		if (run > c - done)
			run = c - done;
		memcpy(dst + done, p->buf + at, run);
		done += run;
	}
	p->rpos += c;
	return (long)c;
}

/* Blocks until all n bytes are written. Fails with EPIPE (there are no
 * signals, so no SIGPIPE) if the read end is closed before anything was
 * written; otherwise a reader vanishing mid-write yields a short count. */
long pipe_write(struct pipe *p, const char *src, u64 n)
{
	u64 done = 0;
	while (done < n) {
		if (!p->rd_open)
			return done ? (long)done : -EPIPE;
		u64 space = PIPE_SIZE - (p->wpos - p->rpos), left = n - done;
		/* small writes must not interleave with other writers */
		if (!space || (n <= PIPE_BUF && space < left)) {
			yield();
			continue;
		}
		u64 c = left < space ? left : space;
		for (u64 k = 0; k < c;) {
			u64 at = (p->wpos + k) % PIPE_SIZE,
			    run = PIPE_SIZE - at;
			if (run > c - k)
				run = c - k;
			memcpy(p->buf + at, src + done + k, run);
			k += run;
		}
		p->wpos += c;
		done += c;
	}
	return (long)n;
}
