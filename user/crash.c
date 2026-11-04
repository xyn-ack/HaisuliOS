#include "ulib.h"
int main(void)
{
	printf("about to dereference NULL...\n");
	*(volatile int *)0 = 1;
	return 0;
}
