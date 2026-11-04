#include "ulib.h"
#define MAXTOK 32
#define MAXSTAGE 8

/* Child side: exec argv (bare names are looked up in /bin). Never returns. */
static void exec_cmd(char **argv)
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
	execve(path, argv, 0);
	dprintf(2, "sh: %s: not found\n", cmd);
	exit(127);
}

/* Run "cmd args | cmd args | ..." (argv holds the tokens, "|" included);
 * returns the exit status of the last stage. */
static int run(char **argv, int argc)
{
	char **stage[MAXSTAGE];
	int nstage = 0, pids[MAXSTAGE], started = 0;
	stage[nstage++] = argv;
	for (int i = 0; i < argc; i++)
		if (!strcmp(argv[i], "|")) {
			if (nstage == MAXSTAGE) {
				dprintf(2, "sh: too many pipeline stages\n");
				return 1;
			}
			argv[i] = 0;
			stage[nstage++] = argv + i + 1;
		}
	for (int i = 0; i < nstage; i++)
		if (!stage[i][0]) {
			dprintf(2, "sh: syntax error near '|'\n");
			return 2;
		}

	int in = -1; /* read end of the pipe feeding the next stage */
	for (int i = 0; i < nstage; i++) {
		int pfd[2] = { -1, -1 };
		if (i + 1 < nstage && pipe(pfd) < 0) {
			perror("sh: pipe");
			break;
		}
		int pid = fork();
		if (pid < 0) {
			perror("sh: fork");
			if (pfd[0] >= 0) {
				close(pfd[0]);
				close(pfd[1]);
			}
			break;
		}
		if (pid == 0) {
			if (in >= 0) {
				dup2(in, 0);
				close(in);
			}
			if (pfd[1] >= 0) {
				dup2(pfd[1], 1);
				close(pfd[0]);
				close(pfd[1]);
			}
			exec_cmd(stage[i]);
		}
		pids[started++] = pid;
		/* the shell must drop its copies, or readers never see EOF */
		if (in >= 0)
			close(in);
		if (pfd[1] >= 0)
			close(pfd[1]);
		in = pfd[0];
	}
	if (in >= 0)
		close(in);

	int rc = 1;
	for (int i = 0; i < started; i++) {
		int st = 0;
		waitpid(pids[i], &st, 0);
		const char *cmd = stage[i][0];
		if (st & 0x7f) {
			printf("sh: %s killed by signal %d\n", cmd, st & 0x7f);
			rc = 128 + (st & 0x7f);
		} else
			rc = (st >> 8) & 0xff;
	}
	return started == nstage ? rc : 1;
}

/* Split a line into words; "|" is always a token of its own. */
static int tokenize(const char *p, char *buf, char **argv)
{
	int argc = 0;
	for (;;) {
		while (*p == ' ' || *p == '\t' || *p == '\n')
			p++;
		if (!*p)
			break;
		if (argc == MAXTOK)
			return -1;
		argv[argc++] = buf;
		if (*p == '|')
			*buf++ = *p++;
		else
			while (*p && *p != ' ' && *p != '\t' && *p != '\n' &&
			       *p != '|')
				*buf++ = *p++;
		*buf++ = 0;
	}
	argv[argc] = 0;
	return argc;
}

int main(void)
{
	char line[256], words[2 * sizeof line], *argv[MAXTOK + 1];
	printf("HaisuliOS shell. Try: ls /bin, cat /etc/motd, hello, crash, "
	       "pipetest, ls /bin | cat, exit\n");
	for (;;) {
		printf("$ ");
		long n = read(0, line, sizeof line - 1);
		if (n <= 0) {
			printf("\n");
			return 0;
		}
		line[n] = 0;
		int argc = tokenize(line, words, argv);
		if (argc < 0) {
			dprintf(2, "sh: too many arguments\n");
			continue;
		}
		if (!argc)
			continue;
		if (!strcmp(argv[0], "exit"))
			return argc > 1 ? argv[1][0] - '0' : 0;
		run(argv, argc);
	}
}
