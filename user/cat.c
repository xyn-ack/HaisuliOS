#include "ulib.h"
static int dump(int fd)
{
	char b[256];
	long n;
	while ((n = read(fd, b, sizeof b)) > 0)
		write(1, b, n);
	return n < 0;
}
int main(int argc, char **argv)
{
	if (argc < 2)
		return dump(0);
	int rc = 0;
	for (int i = 1; i < argc; i++) {
		int fd = open(argv[i], 0);
		if (fd < 0) {
			printf("cat: %s: cannot open (%d)\n", argv[i], fd);
			rc = 1;
			continue;
		}
		rc |= dump(fd);
		close(fd);
	}
	return rc;
}
