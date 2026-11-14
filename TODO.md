# TODO

Where HaisuliOS is going: its goals and the roadmap toward them. What has been
done so far is in [CHANGELOG.md](CHANGELOG.md), and what the system can do
today in the [README](README.md).

## Goals

- **A POSIX system.** Enough of POSIX that a libc (musl) can be ported on top
  of the ABI, and ordinary programs run on it.
- **A true microkernel.** The kernel provides mechanisms only; policy lives
  in user-space servers. POSIX processes, signals and `execve` already do
  (the process server), and so do the terminal (the console server) and the
  job control policy (the shell). Next to move out: the file system and the
  file descriptors.
- **User-space servers for the rest:**
  - a VFS, and file system servers behind it;
  - libc (a musl port) on the ABI;
  - more device drivers.
- **Safe and small,** as so far: no external crates, stable Rust, and unsafe
  code confined to a few small modules with safe interfaces.

## Roadmap toward POSIX

1. Timer (LAPIC/PIT) + preemption. (Upcalls are entered at the end of
   system calls, which is enough only while scheduling is cooperative; with
   preemption, returns from interrupts need the same `return_to_user` step.)
2. Higher-half kernel, kernel heap, copy-on-write `fork`.
3. More programs in `/bin`, moving commands out of the shell; a `PATH`.
4. Capability-based IPC endpoints; shared memory / page grants.
5. More user-space servers: a VFS, with the RAM file system moving out of the
   kernel into a file system server; then port musl libc.
6. Move the fd table out of the kernel (libc/VFS), so that `read`/`write`
   reach servers directly rather than via the kernel.

## Smaller items

Known limitations (see the README) that are worth fixing on the way:

- An init that is not the shell, and that reaps every orphan, including the
  zombies it did not start.
- `SIGCHLD`.
- Restarting a system server that dies (like MINIX's reincarnation server).
- `execve`: close-on-exec, `#!` scripts.
- The file system: `unlink`, `mkdir`, `lseek`, file permissions.
- The shell: double quotes, `;` and pipes.
