#include "ulib.h"
int main(int argc, char **argv)
{
	const char *p = argc > 1 ? argv[1] : "/";
	int fd = open(p, 0);
	if (fd < 0) {
		printf("ls: %s: no such file or directory\n", p);
		return 1;
	}
	char buf[512] __attribute__((aligned(8)));
	long n;
	while ((n = getdents64(fd, buf, sizeof buf)) > 0)
		for (long o = 0; o < n;) {
			struct linux_dirent64 *d = (void *)(buf + o);
			printf("%s%s\n", d->d_name, d->d_type == 4 ? "/" : "");
			o += d->d_reclen;
		}
	if (n == -20)
		printf("%s\n", p); /* ENOTDIR: it's a plain file */
	return 0;
}
