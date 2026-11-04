#include "ulib.h"
int main(void)
{
	printf("hello from pid %d (parent %d)\n", getpid(), getppid());
	char *heap = sbrk(8192);
	if (heap == (char *)-1) {
		printf("sbrk failed\n");
		return 1;
	}
	memcpy(heap, "heap works", 11);
	printf("sbrk: %s\n", heap);
	for (int i = 0; i < 3; i++) {
		int pid = fork();
		if (pid == 0) {
			printf("  child %d (pid %d) running\n", i, getpid());
			exit(10 + i);
		}
	}
	int st, pid;
	while ((pid = waitpid(-1, &st, 0)) > 0)
		printf("reaped pid %d, exit status %d\n", pid,
		       (st >> 8) & 0xff);
	return 0;
}
