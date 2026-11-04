#include "ulib.h"
static int dump(int fd)
{
	char b[256];
	long n;
	while ((n = read(fd, b, sizeof b)) > 0)
		if (write(1, b, n) != n)
			return 1; /* e.g. EPIPE: the reader went away */
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
			dprintf(2, "cat: %s: %s\n", argv[i], strerror(errno));
			rc = 1;
			continue;
		}
		rc |= dump(fd);
		close(fd);
	}
	return rc;
}
