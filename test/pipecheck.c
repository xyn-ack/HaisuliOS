/* Pipe edge cases: EOF, EPIPE, blocking across processes, fd/slot exhaustion. */
#include "ulib.h"

static int fails;
static void check(int ok, const char *what)
{
	printf("%s: %s\n", ok ? "ok  " : "FAIL", what);
	if (!ok)
		fails++;
}

/* child streams `total` bytes of a counting pattern in uneven chunks */
static void producer(int wfd, long total)
{
	char b[1500];
	long sent = 0;
	int chunk = 1;
	while (sent < total) {
		long n = total - sent < chunk ? total - sent : chunk;
		for (long i = 0; i < n; i++)
			b[i] = (char)((sent + i) % 251);
		if (write(wfd, b, n) != n)
			exit(1);
		sent += n;
		chunk = chunk * 7 % 1499 + 1;
	}
	exit(0);
}

int main(void)
{
	int p[2], st;
	char b[64];

	check(pipe(p) == 0 && p[0] >= 0 && p[1] >= 0 && p[0] != p[1],
	      "pipe() returns two distinct fds");
	check(write(p[1], "abc", 3) == 3, "write to pipe");
	check(read(p[1], b, 1) == -1 && errno == EBADF,
	      "read from write end fails");
	check(write(p[0], "x", 1) == -1 && errno == EBADF,
	      "write to read end fails");
	close(p[1]);
	check(read(p[0], b, sizeof b) == 3 && !memcmp(b, "abc", 3),
	      "buffered data survives closing the write end");
	check(read(p[0], b, sizeof b) == 0, "EOF once writers are gone");
	close(p[0]);

	pipe(p);
	close(p[0]);
	check(write(p[1], "x", 1) == -1 && errno == EPIPE,
	      "EPIPE with no readers");
	close(p[1]);

	/* 100 KB through a 4 KB buffer: both sides must block and wrap around */
	const long total = 100000;
	pipe(p);
	int pid = fork();
	if (pid == 0) {
		close(p[0]);
		producer(p[1], total);
	}
	close(p[1]);
	long got = 0, n;
	int good = 1;
	char big[777];
	while ((n = read(p[0], big, sizeof big)) > 0) {
		for (long i = 0; i < n; i++)
			if (big[i] != (char)((got + i) % 251))
				good = 0;
		got += n;
	}
	close(p[0]);
	waitpid(pid, &st, 0);
	check(good && got == total && n == 0 && st == 0,
	      "100000 bytes parent<-child, in order, then EOF");

	/* a writer blocked on a full pipe is released when the reader exits */
	pipe(p);
	pid = fork();
	if (pid == 0) {
		close(p[1]);
		read(p[0], b, 1);
		exit(0);
	}
	close(p[0]);
	static char fill[10000];
	long w1 = write(p[1], fill, sizeof fill);
	long w2 = write(p[1], fill, 1);
	int e2 = errno;
	close(p[1]);
	waitpid(pid, &st, 0);
	check(w1 > 0 && w1 < (long)sizeof fill,
	      "short write when reader exits");
	check(w2 == -1 && e2 == EPIPE, "then EPIPE");

	/* fd table exhaustion: a half-installed pipe must be rolled back */
	int fds[16], nfd = 0, r;
	while ((r = pipe(p)) == 0 && nfd < 16) {
		fds[nfd++] = p[0];
		fds[nfd++] = p[1];
	}
	check(r == -1 && errno == EMFILE, "EMFILE when the fd table is full");
	int spare = dup(0);
	check(spare >= 0, "failed pipe() leaked no descriptor");
	if (spare >= 0)
		close(spare);
	for (int i = 0; i < nfd; i++)
		close(fds[i]);

	/* pipe slots and buffers are released on close */
	int leak = 0;
	for (int i = 0; i < 200; i++) {
		if (pipe(p) != 0) {
			leak = 1;
			break;
		}
		close(p[0]);
		close(p[1]);
	}
	check(!leak, "200 pipe/close cycles (no slot leak)");

	check(pipe((int *)0) == -1 && errno == EFAULT,
	      "EFAULT for bad pointer");

	printf("pipecheck: %s (%d failure%s)\n", fails ? "FAILED" : "passed",
	       fails, fails == 1 ? "" : "s");
	return fails != 0;
}
