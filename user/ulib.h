#ifndef ULIB_H
#define ULIB_H
typedef unsigned long size_t;
typedef long ssize_t;
/* Wrappers return the raw kernel result: >= 0 on success, -errno on failure. */
long write(int fd, const void *buf, size_t n);
long read(int fd, void *buf, size_t n);
int open(const char *path, int flags);
int close(int fd);
int dup2(int oldfd, int newfd);
int fork(void);
int execve(const char *path, char *const argv[], char *const envp[]);
int waitpid(int pid, int *status, int options);
void exit(int status) __attribute__((noreturn));
int getpid(void);
int getppid(void);
long getdents64(int fd, void *buf, size_t n);
void *sbrk(long inc);
void yield(void);
size_t strlen(const char *s);
int strcmp(const char *a, const char *b);
char *strchr(const char *s, int c);
void *memcpy(void *d, const void *s, size_t n);
void *memset(void *d, int c, size_t n);
int printf(const char *fmt, ...);
struct linux_dirent64 {
	unsigned long d_ino;
	long d_off;
	unsigned short d_reclen;
	unsigned char d_type;
	char d_name[];
} __attribute__((packed));
#endif
