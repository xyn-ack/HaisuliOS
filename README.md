# HaisuliOS — a bare-bones POSIX-flavoured kernel for x86_64

~900 lines of C and assembly. Boots via Multiboot, enters long mode, runs real
ring-3 processes with separate address spaces, and exposes a Linux-numbered
syscall ABI.

## Build & run
    make            # needs gcc, binutils, make, tar
    make run        # needs qemu-system-x86_64; exit with Ctrl-A then X

(`make run` = `qemu-system-x86_64 -kernel build/kernel32.elf -m 128M -nographic`)
Console is the serial port (COM1), which `-nographic` wires to your terminal.

## What's in it
| Area        | Implementation |
|-------------|----------------|
| Boot        | Multiboot1, 32→64-bit trampoline, identity map of 1 GB (2 MB pages) |
| CPU         | GDT + TSS, IDT for exceptions 0–31, SYSCALL/SYSRET fast path |
| Memory      | Free-list page allocator, 4-level paging, per-process page tables |
| Processes   | fork (eager copy), execve (ELF64), exit, wait4, getpid/getppid, brk |
| Scheduling  | Cooperative round-robin (switches in wait/read/exit/sched_yield) |
| Files       | Read-only ustar tar initrd, fds, open/read/write/close/lseek/dup/dup2/getdents64 |
| TTY         | Canonical line discipline on serial: echo, backspace, Ctrl-D EOF |
| Userland    | init, sh, ls, cat, echo, hello (fork/wait/sbrk demo), crash (fault isolation demo) |

Syscalls (Linux x86_64 numbers): read 0, write 1, open 2, close 3, lseek 8,
brk 12, sched_yield 24, dup 32, dup2 33, getpid 39, fork 57, execve 59,
exit 60, wait4 61, getppid 110, getdents64 217, exit_group 231.
Errors are returned as -errno, as in the raw Linux ABI.

## Memory map
    0x0000_0000 – 0x3FFF_FFFF  kernel + RAM, identity-mapped, supervisor-only
    0x4000_0000 – 0x7FFF_FFFF  user space (ELF linked at 0x40001000, stack at top)

## Known limitations (the roadmap)
- No pipes, signals, writable filesystem, cwd/chdir, stat, env vars, or job control
- No timer/IRQs: scheduling is cooperative; a user spin loop hangs the machine
- Page tables are copied eagerly on fork (no copy-on-write); no demand paging
- Single CPU, assumes ≤ 1 GB RAM, kernel stack is 4 KB per process
- Not yet a drop-in target for musl/newlib; a libc port needs the missing calls
  above (fstat, mmap, ioctl, clock_gettime, ...)

Suggested next steps: PIC/APIC timer for preemption → pipes + dup2 redirection in
`sh` → signals → writable tmpfs → copy-on-write fork → port musl.
