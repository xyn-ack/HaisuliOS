# Architecture

How HaisuliOS works inside: the layout of the source, the kernel's mechanisms and the user-space
servers built on them. The [README](../README.md) has the overview and how to build and run it;
[TESTING.md](TESTING.md) how it is tested.

* [Source layout](#source-layout)
* [Unsafe code](#unsafe-code)
* [Boot](#boot)
* [CPUs and threads](#cpus-and-threads)
* [Virtual address space (per task)](#virtual-address-space-per-task)
* [Syscall ABI](#syscall-abi)
* [Processes](#processes)
* [Programs](#programs)
* [Files](#files)
* [Console server](#console-server)
* [Signals and job control](#signals-and-job-control)

## Source layout

```
kernel/            the kernel
  main.rs          kmain, panic handler; denies unsafe code crate-wide
  arch/            hardware layer: the only place with unsafe Rust (besides sync.rs)
    boot.rs        Multiboot entry, 32->64-bit switch, boot page tables (asm)
    cpu.rs         privileged instructions; port I/O on a fixed list of devices
    percpu.rs      each CPU's own block, through GS (swapgs on every user/kernel switch)
    apic.rs        local APICs: the tick, IPIs, starting CPUs; the PIT as a stopwatch
    acpi.rs        the CPUs the firmware's MADT lists
    smp.rs         starting the other CPUs: the real-mode trampoline (asm)
    entry.rs       exception/IRQ/syscall entry stubs, return to user mode (asm)
    context.rs     kernel stacks, per-CPU idle contexts, context switch
    phys.rs        physical memory access, limited to allocator-owned frames
    multiboot.rs   boot-loader information: memory map, boot modules
    x86.rs         names for the CPU's numbers: register bits, vectors, descriptors
  sync.rs          Lock: a ticket spinlock that panics on re-entrant use
  gdt.rs           GDT + a TSS per CPU (layout chosen for SYSCALL/SYSRET), I/O bitmap
  idt.rs           IDT, CPU exception handling
  irq.rs           8259 PIC, forwarding IRQs to drivers; every interrupt's dispatch
  pmm.rs           Physical frame allocator (Multiboot memory map)
  vmm.rs           Paging: map, translate, clone (task_fork), destroy, user-memory copies
  elf.rs           ELF64 loader for user programs
  program.rs       a program's address space: its image, stack and arguments
  fs.rs            RAM file system (the unpacked ramdisk), open file descriptions
  task.rs          Tasks and threads, the scheduler of every CPU, task control, upcalls,
                   mmap, IPC, services
  upcall.rs        The frame that enters a handler (Linux signal frame layout)
  syscall.rs       Syscall dispatcher (Linux-compatible numbers)
  serial.rs        COM1 kernel debug log (and the console's transmitter), print!/println!
abi/               syscall numbers, errno values, IPC protocols, the 16550 UART's
                   registers (kernel + user)
user/rt/           user runtime: entry point, syscall wrappers, POSIX process calls
                   (requests to the process server), print!, panics
  src/thread.rs    threads: spawn and join, on the kernel's clone and exit
user/proc/         process server: POSIX processes and signals
  src/signals.rs   dispositions, default actions, signal sets
user/console/      console server (/sbin/console)
user/coreutils/    programs for /bin: echo, true, false, nproc
user/probe_test/   /bin/probe_test: a test program for what the shell cannot reach
user/smp_test/     /bin/smp_test: a test program for multiple CPUs and threads
user/init/         init (pid 1): /etc/motd, then a job control shell
  src/words.rs     command line words: quotes, `$?`, `&`, redirection operators
  src/redirect.rs  applying (and undoing) redirections
  src/traps.rs     the shell's signal handling and `trap`
user/user.ld       linker script for user programs
docs/              this file and TESTING.md
rootfs/            contents of the boot ramdisk (rootfs/etc/motd = /etc/motd), besides
                   the programs in /sbin and /bin, which the build adds
test/              tests (see TESTING.md)
```

## Unsafe code

Unsafe Rust is used only where nothing else can do the job: privileged
instructions, the entry and context-switch assembly, and memory that the
boot loader or the hardware defines. It lives in a few small modules that
wrap it in safe interfaces. The restrictions of those interfaces are what
make them safe:

* `kernel/arch/`: port I/O only on a fixed list of devices that cannot do
  DMA, with bounds-checked register offsets. Physical memory access only to
  frames the allocator owns; `phys` itself refuses anything in the kernel
  image, in boot-loader data or below 1 MiB. A context switch only to
  contexts that the context module itself saved, and never to one that
  another CPU has not finished leaving. (On top of that, `vmm::map_user`
  maps only allocator frames, so user space never sees kernel memory.) The
  local APIC's registers only at their fixed offsets, in the page the boot
  page tables map. Firmware tables (ACPI) only where no allocator frame or
  kernel data lies, read-only. The CPU trampoline only in its one page of
  conventional memory, which nothing else uses. The GS base only as a
  pointer to the CPU's own block.
* `kernel/sync.rs`: `Lock`, a ticket spinlock, which guards the task table,
  the TSSs, the file system, the serial line and the page-table changes (all
  other shared kernel state is atomics). Kernel code runs with interrupts
  disabled, so only another CPU can hold it while this one waits; if the
  same CPU finds it held, that is re-entrant use, which panics instead of
  deadlocking (or creating aliased `&mut`).
* `user/rt`: the `syscall` instruction and the start-up code, which reads
  the arguments only within the initial stack area. The wrappers
  pass the kernel only pointers taken from borrows of the right length and
  mutability (`&mut` whenever the kernel or an IPC server may write). A
  `write()` buffer can be a shared borrow because the kernel enforces that
  the console server only reads it (see below). `thread` starts a thread
  with `clone` on a stack that no Rust object lives in, and ends it without
  touching that stack once it has said so.
* `user/console`: its UART's port I/O (its own 8 registers).
* `user/proc`: handing out its process table, which is too big for the stack,
  as a `&'static mut` from a `static`: once, which an atomic flag enforces.

Every other kernel module is checked by the compiler: `main.rs` sets
`#![deny(unsafe_code)]` and allows it only for `arch` and `sync`. `init` is
`#![forbid(unsafe_code)]`. Every unsafe block carries a `SAFETY:` comment
(`clippy::undocumented_unsafe_blocks` is denied). The kernel also relies on
compiler-builtins for `memcpy` and friends instead of defining them itself.

## Boot

The boot loader loads the kernel and a list of **boot modules**: only what it
takes to load programs from the file system. Every module that is an ELF file
is started as a task, in order: init and the process server get task ids 1
and 2. The process server adopts the boot tasks as processes under the same
ids, so init is pid 1. A module that is not an ELF file is the **ramdisk**, a
ustar archive: the kernel unpacks it into its RAM file system (see *Files*
below), so `/etc/motd` is an ordinary file that init opens, and the programs
in `/sbin` and `/bin` are ordinary files too.

The other servers are not critical to booting, so they come from the file
system: the process server starts the **system servers** listed in
`/etc/servers`, one path per line. Today that is the console server,
`/sbin/console`, which becomes pid 3. Like a boot module, a system server has
no parent and leads its own session and process group. init waits for the
console while it starts: a terminal `read` or `write` waits for the console
service to register.

Before it starts any task, the boot CPU starts the others (see *CPUs and
threads*) and logs how many run: `[kernel] 4 CPUs online`.

Boot modules and system servers are *privileged*: they may use `ioperm`,
`irq_bind`, `svc_register`, `serial_write` and `power_off`. Nothing else is:
neither copies made with `task_fork` (forked children) nor programs started
with `execve`.

When init exits, the process server powers the machine off.

## CPUs and threads

**CPUs.** The boot CPU finds the others in the firmware's ACPI tables (the
MADT's enabled local APICs, up to 16 CPUs) and starts them one at a time with
the INIT / start-up IPI sequence: each wakes in 16-bit real mode in a
*trampoline* that the boot CPU copied below 1 MiB, switches to long mode with
the boot page tables, and enters Rust on a stack of its own. Every CPU has its
own GDT entry and TSS (its kernel stack for interrupts, its I/O bitmap), its
own `syscall` stack, and a small *per-CPU block* that its GS base points to in
the kernel: every entry from user mode does `swapgs`, every return does it
again. A CPU that takes too long to start is given up for good (it halts if it
comes late), and no more are started.

Each CPU's local APIC runs a periodic timer, the scheduler's *tick* (10 ms,
calibrated against the PIT), and carries the *reschedule IPI*, with which one
CPU makes another look at the scheduler. Device IRQs still come from the 8259
PICs, to the boot CPU only. The serial line has one transmitter, the kernel's:
its log and the console server's output (`serial_write`) each go out in one
piece, however many CPUs write at once.

**Threads.** A task is an address space (with its file descriptors, mailbox,
services and privileges) and one or more threads. `clone` (with exactly
`CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD`: a thread,
not a process) starts another thread in the caller's task, which returns 0 on
the new stack; `exit` ends the calling thread, and when the last one ends,
the task ends as with `task_exit`. Thread and task ids share one range: a
task's id is the id of its first thread, and stays taken until the task is
gone. Ids are handed out round robin, so an id that was just freed is the last
to be reused. The runtime's `thread::spawn` and `join` are built on these
calls (see *Processes* for what they mean to POSIX).

**Scheduling.** Every thread is in one table; each CPU takes the next runnable
thread after the one it ran last (round robin). A thread runs until it blocks
or yields; after its quantum (two ticks) the tick preempts it on its way back
to user mode, if another thread waits. A thread that becomes runnable wakes
an idle CPU with an IPI, except when the thread that made it runnable is about
to give up its own CPU, which then runs it: a client's call goes straight to
the server (a direct hand-off), and a server's reply, the program after
`execve`, a new task or thread wake an idle CPU only if their maker is still
running at the next tick. A CPU with nothing to run halts in its own *idle
context* (its boot stack), until a reschedule IPI, or every tenth tick, makes
it look again. The last step on every way back to
user mode, at the end of a system call or of an interrupt that arrived in user
mode, is where a thread ends if its task has ended, waits while its task is
stopped, gives way when preempted, and enters an upcall.

The scheduler's state is behind one lock, which every CPU takes: so the
frequent paths (the tick, an interrupt's return to user mode, an idle CPU
waking up) first look at per-CPU hints (atomics) and skip the lock when
there is nothing to do, and IPIs go out only once the lock is released (an
IPI is a device access, which can be slow). The lock is a ticket lock, handed
out in order: a CPU that retakes it in a loop cannot starve the others.

**Ending tasks.** A task never vanishes from under a CPU. Ending one (a kill,
a fault, `task_exit`, or the exit of its last thread) makes it *dying*: its
threads that are on no CPU end at once; the others are marked, their CPUs
interrupted, and they end themselves at their next return to user mode or
when they block. Its calls fail and its services go at once, but its address
space and file descriptors are freed only once none of its threads is on a
CPU any more (a CPU switches to another address space before it lets go of a
thread's context) and no other task is copying to or from its memory (an IPC
copy *pins* the task). `execve` ends the other threads of the task the same
way, and frees the old address space once they are gone.

## Virtual address space (per task)

```
PML4[0]       kernel: identity map of first 1 GiB (shared, supervisor-only)
PML4[1..256]  user (private):
  0x0000_0080_0000_0000  program image   (ELF segments: R-X / R-- / RW-)
  0x0000_0100_0000_0000  mmap() region   (RW-, NX unless PROT_EXEC)
  0x0000_7FFF_FFFF_F000  top of stack    (RW-, NX, 32 KiB, grows down)
```

User pages are mapped 4 KiB at a time, so `task_fork` is a deep copy (no COW yet).
W^X is enforced for the image/stack/mmap via the NX bit (EFER.NXE is on); the
ELF loader refuses segments that are both writable and executable.

## Syscall ABI

`syscall` instruction; `rax` = number; args in `rdi rsi rdx r10 r8 r9`;
return in `rax` (negative errno on failure). File and memory calls use
**Linux x86_64 numbers**. All numbers and constants live in `abi/src/lib.rs`.
There are no process calls: `fork`, `exit`, `wait4`, `getpid`, `kill`,
`sigaction` and the rest are requests to the process server (see *Processes*).

| Nr     | Call                              | Notes                                                   |
|--------|-----------------------------------|---------------------------------------------------------|
| 0      | `read(fd,buf,len)`                | a file, or the terminal (one line per read)             |
| 1      | `write(fd,buf,len)`               | a file, or the terminal                                 |
| 2      | `open(path,flags,mode)`           | `O_CREAT`/`EXCL`/`TRUNC`/`APPEND`/`DIRECTORY`; no modes |
| 3      | `close(fd)`                       |                                                         |
| 9      | `mmap(_,len,prot,...)`            | anonymous only; kernel chooses the address              |
| 15     | `rt_sigreturn()`                  | from an upcall's restorer; restores the saved registers |
| 24     | `sched_yield()`                   | lets the other ready threads run                        |
| 32     | `dup(fd)`                         | lowest free descriptor; shares the open file description |
| 33     | `dup2(fd,newfd)`                  |                                                         |
| 72     | `fcntl(fd,cmd,arg)`               | `F_DUPFD` only                                          |
| 173    | `ioperm(from,num,on)`             | privileged; ports `0..0x400` only                       |
| 56     | `clone(flags,stack,...)`          | a thread in the caller's task (`clone::THREAD_FLAGS` only) |
| 60     | `exit(status)`                    | ends the calling thread (the last one ends the task)    |
| 186    | `gettid()`                        | the calling thread's id                                 |
| 204    | `sched_getaffinity(0,len,mask)`   | the online CPUs, one bit each                           |
| 217    | `getdents64(fd,buf,len)`          | Linux `struct linux_dirent64` records                   |
| 309    | `getcpu(&cpu,&node,_)`            | the CPU the caller runs on                              |
| 0x1000 | `ipc_send(tid,a,b)`               | async; non-blocking; `-EAGAIN` if mailbox (8) is full   |
| 0x1001 | `ipc_recv(&[u64;4])`              | blocking; `[sender, a, b, task]` (see below)            |
| 0x1002 | `ipc_call(tid,op,buf,len)`        | synchronous; blocks until the server replies            |
| 0x1003 | `ipc_reply(client,status)`        | completes a received call; client gets `status`         |
| 0x1004 | `ipc_read(client,off,dst,len)`    | copy from the client's call buffer (if the call allows) |
| 0x1005 | `ipc_write(client,off,src,len)`   | copy into the client's call buffer (if the call allows) |
| 0x1006 | `irq_bind(line)`                  | privileged; route an IRQ line to the caller             |
| 0x1007 | `irq_ack(line)`                   | unmask the line after servicing the device              |
| 0x1008 | `svc_register(id)`                | privileged; become a well-known service (1 = console, 2 = processes) |
| 0x1009 | `svc_lookup(id)`                  | task id of a registered service                         |
| 0x100A | `debug_log(buf,len)`              | kernel debug log; never blocks (used for panics)        |
| 0x100D | `svc_wait(id)`                    | `svc_lookup`, but waits until the service registers     |
| 0x100E | `task_self()`                     | the caller's task id                                    |
| 0x1010 | `task_exit()`                     | ends the calling task; reported to the task manager     |
| 0x1011 | `task_fork(thread)`               | task manager; copies the task of a thread blocked in a call to it, with only that thread |
| 0x1012 | `task_kill(tid)`                  | task manager; ends a task                               |
| 0x1013 | `task_stop(tid)`                  | task manager; stops scheduling a task                   |
| 0x1014 | `task_cont(tid)`                  | task manager; schedules a stopped task again            |
| 0x1015 | `task_upcall(tid,&[u64;6])`       | task manager; enter a handler on the way to user mode   |
| 0x1016 | `task_info(tid,buf,len)`          | a `TaskInfo`: running, blocked or stopped; its name     |
| 0x1017 | `power_off()`                     | privileged                                              |
| 0x1018 | `task_exec(thread,blk,len,flags)` | task manager; the task of a thread blocked in a call to it runs a program from the file system (its other threads end) |
| 0x1019 | `task_spawn(blk,len,flags)`       | task manager; a program from the file system in a new task |
| 0x101A | `serial_write(buf,len)`           | privileged; raw bytes out of the serial line, in one piece (the console's output) |

Task ids are the kernel's; pids are the process server's. IPC addresses
tasks, and any thread of a server may receive its calls; a call is made by a
thread, and the reply goes to that thread. The *task manager* is whichever task registered as service 2, the
process server: only it may use the task control calls (`task_fork` to
`task_upcall`), and the kernel reports to it the tasks that end by
themselves.

`ipc_recv` delivers, in order: IRQ notifications `[0, KMSG_IRQ, line, 0]`
(sender 0 is the kernel and cannot be forged), for the task manager the
reports of ended tasks (`[0, KMSG_TASK_EXIT, tid, 0]` after `task_exit` or the
end of its last thread, `[0, KMSG_TASK_FAULT, tid, 0]` after a CPU exception),
mailbox messages `[task, a, b, task]`, then the oldest pending call
`[client thread, op, len, client task]`. A server may keep a call pending,
without replying, for as long as it likes. The console server does this for
`read` until a full line has been typed, and the process server for `pause`
and `wait4`. If a server exits, its clients' calls fail with `-EPIPE`.

While a call is pending, the server can only reach the client's buffer
through `ipc_read` / `ipc_write`, and the kernel limits even that to what the
call allows. An explicit `ipc_call` lets the server read and write the
buffer. For the terminal `write` and `read` calls it forwards, the kernel
lets the console server only *read* a `write()` buffer and only *write* a
`read()` buffer. Anything else fails with `-EACCES`, so not even a buggy or
malicious console server can modify memory that a writer only lent it, or
see what a reader's buffer held before.

## Processes

POSIX processes live in user space, in the **process server** (`user/proc`),
like the terminal lives in the console server. The kernel only provides the
mechanisms it needs: tasks, and the task control calls above. The process
server keeps the process table: pids (the lowest free one, not reused while it
names a process group or session), parents and children, process groups and
sessions, exit statuses and signal state. The user runtime (`user/rt`) turns
the POSIX calls into `ipc_call`s to it; the protocol, `abi::proc`, passes a
few `u64` words per request:

* `fork`: the server has the kernel copy the caller's task with only the
  calling thread (`task_fork`, whose copy returns 0 from the pending call)
  and records the child, with its parent's process group, session and signal
  dispositions.
* `exit`, a signal's default action, a fault: the server ends the task, with
  all its threads (`task_kill`; a fault has ended it already) and the process
  becomes a **zombie**, which is only a record in the server with its POSIX
  wait status. The task's memory and kernel slot are freed as soon as no CPU
  uses them any more. The server
  logs the end, e.g. `[proc] pid 4 (init) exited (wait status 0xf)`,
  reparents the orphans to init and completes the parent's `wait4`.
* `wait4` and `pause` are answered later: the server keeps the call pending,
  so it never blocks on a client. Several threads of a process may wait at
  once; each gets its own answer.
* `execve`: the server has the kernel replace the caller's program with one
  from the file system (`task_exec`; see *Programs*), which also ends the
  process's other threads, and resets the caught signals to their default
  action.
* `getpid`, `getppid`, `setpgid`, `getpgid`, `setsid`, `getsid`, `kill`, the
  terminal's `kill_pgrp` and the listing that `ps` shows are answered from
  the table.

A process is a task with all its threads. The kernel starts threads itself
(`clone`: a mechanism), and the server learns about them as they make
requests: with every call, `ipc_recv` tells it the calling thread, which
gets the reply, and its task, which names the process. So every thread gets
the same `getpid`, `exit` in any thread ends the whole process, and a
`wait4` or `pause` in one thread leaves the others running. Signal
dispositions, masks and pending signals belong to the process (POSIX gives
each thread its own mask; HaisuliOS does not yet). A caught signal goes to a
thread that waits in `pause` or `wait4` in the server, whose wait it ends, or
else to the thread that is about to get a reply from the server, or else to
one the kernel picks (`task_upcall`, see *Signals*). `ps` shows how many
threads each process has (`THR`).

When init exits, the process server powers the machine off. The process
server itself cannot be signalled (`kill` fails with `EPERM`).

## Programs

`execve(path, argv, envp)` runs a program from the file system: a static
x86_64 ELF executable. The runtime sends the process server an *argument
block* (`abi::exec`: the path, the arguments and the environment, at most
4 KiB), and the server passes it to the kernel's `task_exec`. The kernel reads
the file, builds a new address space (the image, a 32 KiB stack and the
arguments), and only then replaces the task's old one, so a failed `execve`
returns to the caller: `ENOENT`, `EACCES` for a directory, `ENOEXEC` for a
file that is not a program, `E2BIG` for too many arguments. The program starts
with the System V initial stack, as on Linux: `argc`, `argv`, `envp` and an
empty auxiliary vector; the runtime hands them to `main` as `StartInfo::args()`
and `env()`.

The process keeps its pid, parent, process group, session, file descriptors
(so redirections carry over), signal mask and ignored signals. It loses its
signal handlers, and with them its upcall, and the kernel-side state of the
old program: I/O permissions, IRQs, services, messages, and privileges.
`task_spawn` is the same for a new task; it is how the process server starts
the system servers, with the `PRIVILEGED` flag, which only a privileged task
may ask for and which `execve` never passes.

The shell runs a command that is not built in as a program: the file it
names if it contains a `/`, or else the file of that name in `/bin` (there is
no `PATH` variable yet). If there is no such file, the shell says so itself
(status 127); otherwise a child runs it (status 126 if it cannot).

```
haisuli# /bin/echo hello world
hello world
[proc] pid 4 (echo) exited (wait status 0x0)
haisuli# false
[proc] pid 4 (false) exited (wait status 0x100)
haisuli# echo $?
1
```

## Files

The kernel keeps a small RAM file system, filled at boot from the boot
ramdisk and writable from then on, like Linux's initramfs: up to 64 files and
directories, of up to 128 KiB each, in page frames from the frame allocator.
Paths are normalised (`//`, `.` and `..`), and there is no current directory,
so a relative path starts at the root. Files can be created (in an existing
directory), written, appended to and truncated; there is no `unlink`,
`mkdir` or `lseek` yet.

File descriptors follow POSIX. They still belong to the kernel's tasks (see
[TODO.md](../TODO.md)): each task has 16, which refer to *open file descriptions* (a
file, an offset and an access mode) in a table of 64 shared by the whole
system. `dup`, `dup2` and `fork` (`task_fork`) make descriptors share a
description, and so its offset, which is what makes `cmd > f 2>&1` write the
output and the errors one after the other. Every boot task starts with
descriptors 0, 1 and 2 on the terminal.

The shell's redirections are built on these: for a command that runs in the
shell itself, the descriptors it redirects are saved (as copies from
descriptor 10 up) and restored afterwards; a job applies its redirections in
the child.

```
haisuli# echo hello > greeting
haisuli# cat /nope greeting > both 2>&1
haisuli# cat < both
cat: /nope: no such file
hello
```

## Console server

`user/console` gets ports `0x3F8..0x3FF` via `ioperm`, binds IRQ 4 and
registers as the console service. It is a canonical-mode terminal: input is
echoed and can be edited (Backspace, Ctrl-U kills the line, Ctrl-C abandons
it, cursor-key escape sequences are ignored) until Enter. A `read` then
returns that line. Ctrl-D on an empty line is end-of-file. Output turns `\n`
into `\r\n`, and goes out through the kernel's transmitter (`serial_write`):
the kernel's log shares the line, and on several CPUs the two would otherwise
cut into each other's lines. The protocol (`OP_WRITE`, `OP_READ`,
`OP_SET_FG_PGRP`) is in `abi/src/lib.rs`.

It is also the *job control terminal*: it remembers a foreground process
group (`OP_SET_FG_PGRP`, the `tcsetpgrp()` of HaisuliOS), and on Ctrl-C it
abandons the line being typed and has the process server send `SIGINT` to
that group (`kill_pgrp`). (Not `kill(-pgid)`: between jobs the foreground
group is the shell's, group 1, and `kill(-1)` is POSIX's broadcast to every
process.) The shell catches `SIGINT` with a signal handler, so its pending
`read` fails with `-EINTR` and it prompts again. The kernel withdraws an
interrupted call, so the server can no longer touch the reader's buffer; it
just finds the reader gone.

## Signals and job control

Signals are implemented by the process server; the kernel provides only the
mechanisms underneath. Every process has a process group and a session
(inherited by `fork`, changed with `setpgid` / `setsid`), a disposition for
every signal and a mask of blocked signals (both inherited by `fork`), and its
pending signals, all kept by the process server.

A signal's *default action* happens as soon as it is sent: the process server
terminates the target (`task_kill`; its wait status is the signal number),
stops it (`task_stop`) or continues it (`task_cont`). POSIX's rules for a stopped process hold: only
`SIGKILL` and `SIGCONT` act on it, anything else stays pending until it is
continued, and `SIGCONT` discards pending stop signals. Terminating a process
wakes its parent out of `wait4`, fails the calls it was serving with `-EPIPE`,
and reparents its children to init. A blocked signal waits until it is
unblocked.

A *caught* signal runs a handler when a thread of its process next returns to
user mode. The process server asks the kernel for an **upcall**
(`task_upcall`), giving the handler, the restorer, the signal, the sender and
the signal mask to restore afterwards (and, with `upcall::THREAD`, the thread
to run it in), and blocks the signal (unless `SA_NODEFER`) and `sa_mask`
meanwhile. It asks before it replies to a request of the process, so that the
handler has run by the time the call returns, even when the reply reaches the
thread on another CPU at once (as POSIX requires of `sigprocmask` and of a
`kill` to oneself). The kernel enters an upcall on the thread's way back to
user mode, at the end of a system call or of an interrupt: a thread busy on
another CPU is interrupted for it. It pushes a frame on the user stack and
enters the handler with the signal number, a `siginfo_t` and a
`ucontext_t`. The frame has the Linux x86_64 layout, so a libc ported later
finds things where it expects them. The handler returns to its
`SA_RESTORER`, which hands the saved mask back to the process server and
calls `rt_sigreturn` to restore the interrupted registers. The kernel treats
that context as untrusted: it keeps the user code and stack segments, only
takes the arithmetic flags from it, and a frame that cannot be read or
written, or that would resume at a kernel address, is a fault: the process
dies of `SIGSEGV`. The kernel holds one upcall per task at a time; further
caught signals wait in the process server until the handler returns.

A process asleep in `pause` or `wait4` (in the process server), or in
`ipc_recv` or a console `read` (in the kernel), is woken by a caught signal,
and the call fails with `-EINTR`, or is restarted after the handler with
`SA_RESTART` (except `pause`, as POSIX requires). Console writes and other
IPC calls always complete. `SA_RESETHAND` makes a handler one-shot.

The pieces fit together like a Unix terminal session: init is a session
leader (`sid` = `pgid` = 1); every job it starts is a new process group in
that session; the shell hands the terminal to a foreground job with
`tcsetpgrp()` and takes it back afterwards; and the console server sends
Ctrl-C's `SIGINT` to whichever group holds the terminal. The shell catches
`SIGINT` (and ignores the other job control signals), so Ctrl-C interrupts
the job and not the shell; jobs reset those dispositions right after `fork`.
`trap` attaches commands to signals: the shell's one handler only records the
signal, and the commands run between commands, as in any POSIX shell.

```
haisuli# pause &          a job of its own, in process group 4
[1] 4
haisuli# kill -STOP 4     ps then shows it as T (stopped)
haisuli# kill -TERM 4     pending: a stopped process is not terminated yet
haisuli# kill -CONT 4     continued, and the pending SIGTERM arrives
haisuli# wait 4
haisuli# echo $?
143                       128 + SIGTERM, as POSIX requires
haisuli# trap 'echo caught USR1' USR1
haisuli# kill -USR1 $$    the shell's handler runs, then the trap command
caught USR1
```
