#ifndef ULIB_H
#define ULIB_H
typedef unsigned long size_t;
typedef long ssize_t;
/* Wrappers follow POSIX: on failure they return -1 and set errno. */
extern int errno;
#define EPERM 1
#define ENOENT 2
#define E2BIG 7
#define ENOEXEC 8
#define EBADF 9
#define ECHILD 10
#define EAGAIN 11
#define ENOMEM 12
#define EACCES 13
#define EFAULT 14
#define ENOTDIR 20
#define EISDIR 21
#define EINVAL 22
#define ENFILE 23
#define EMFILE 24
#define ESPIPE 29
#define EROFS 30
#define EPIPE 32
#define ENAMETOOLONG 36
#define ENOSYS 38

long write(int fd, const void *buf, size_t n);
long read(int fd, void *buf, size_t n);
int open(const char *path, int flags);
int close(int fd);
int pipe(int fds[2]);
int dup(int oldfd);
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
int memcmp(const void *a, const void *b, size_t n);
int printf(const char *fmt, ...);
int dprintf(int fd, const char *fmt, ...);
char *strerror(int errnum);
void perror(const char *s);
struct linux_dirent64 {
	unsigned long d_ino;
	long d_off;
	unsigned short d_reclen;
	unsigned char d_type;
	char d_name[];
} __attribute__((packed));
#endif
