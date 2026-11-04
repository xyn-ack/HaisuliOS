#include "ulib.h"
static int run(char **argv)
{
	char path[80];
	const char *cmd = argv[0];
	if (strchr(cmd, '/')) {
		size_t n = strlen(cmd);
		if (n > 79)
			n = 79;
		memcpy(path, cmd, n);
		path[n] = 0;
	} else {
		memcpy(path, "/bin/", 5);
		size_t n = strlen(cmd);
		if (n > 70)
			n = 70;
		memcpy(path + 5, cmd, n);
		path[5 + n] = 0;
	}
	int pid = fork();
	if (pid < 0) {
		printf("sh: fork failed (%d)\n", pid);
		return 1;
	}
	if (pid == 0) {
		execve(path, argv, 0);
		printf("sh: %s: not found\n", cmd);
		exit(127);
	}
	int st = 0;
	waitpid(pid, &st, 0);
	if (st & 0x7f) {
		printf("sh: %s killed by signal %d\n", cmd, st & 0x7f);
		return 128 + (st & 0x7f);
	}
	return (st >> 8) & 0xff;
}
int main(void)
{
	char line[256], *argv[16];
	printf("HaisuliOS shell. Try: ls /bin, cat /etc/motd, hello, crash, exit\n");
	for (;;) {
		printf("$ ");
		long n = read(0, line, sizeof line - 1);
		if (n <= 0) {
			printf("\n");
			return 0;
		}
		line[n] = 0;
		int argc = 0;
		char *p = line;
		while (*p && argc < 15) {
			while (*p == ' ' || *p == '\t' || *p == '\n')
				*p++ = 0;
			if (!*p)
				break;
			argv[argc++] = p;
			while (*p && *p != ' ' && *p != '\t' && *p != '\n')
				p++;
		}
		argv[argc] = 0;
		if (!argc)
			continue;
		if (!strcmp(argv[0], "exit"))
			return argc > 1 ? argv[1][0] - '0' : 0;
		run(argv);
	}
}
