#include "ulib.h"
int main(void)
{
	printf("\n[init] pid %d up. Starting shell.\n", getpid());
	for (;;) {
		int pid = fork();
		if (pid == 0) {
			char *av[] = { "sh", 0 };
			execve("/bin/sh", av, 0);
			printf("[init] cannot exec /bin/sh\n");
			exit(127);
		}
		int st, r;
		while ((r = waitpid(-1, &st, 0)) > 0 && r != pid)
			; /* also reaps orphans */
		printf("[init] shell exited, respawning\n");
	}
}
