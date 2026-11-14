# HaisuliOS

A tiny, bare-bones **microkernel** for **x86_64**, written in **Rust**, with a
**POSIX-flavoured** syscall ABI. No external crates, no bootloader crate, no
nightly features: just `core`, inline assembly, and a Multiboot v1 header.

> Status: v0.1. It provides virtual memory, processes, signals with POSIX job
> control, IPC and user-space drivers, and boots into an interactive console
> served by a user-space console server. It is **not** yet a full POSIX system:
> there is no `execve` and no libc, and the file system is a RAM disk.

## What the kernel does (and deliberately doesn't)

| In the kernel (mechanism)                             | In user space (policy)                      |
|-------------------------------------------------------|---------------------------------------------|
| Physical memory manager (bitmap frame allocator)      | **Console server**: COM1 driver, line discipline and the job control terminal (`user/console`) |
| Virtual memory: 4-level paging, per-process spaces    | **init**: motd, then a job control shell (`user/init`) |
| Processes, kernel stacks, context switch, scheduler   | Job control policy: who is in the foreground, what Ctrl-C means |
| `fork`, `exit`, `wait4`, `getpid`, `mmap` (anon)      | Planned: VFS + file system servers          |
| RAM file system (the unpacked ramdisk), file descriptors | Shell redirections (`>`, `>>`, `<`, `2>&1`) |
| Signals, process groups and sessions, stop/continue   | Planned: libc (musl port) on the ABI        |
| IPC: async notifications + synchronous calls with data transfer | Planned: more device drivers      |
| IRQ forwarding, per-process I/O-port permissions      |                                             |
| ELF loader for boot modules; well-known service registry |                                          |
| Exceptions -> fault handling (SIGSEGV-style kill)     |                                             |

The kernel contains no device drivers apart from a write-only debug log on
COM1. A process's file descriptors refer to files in the kernel's RAM file
system or to the terminal; `read` and `write` on the terminal are forwarded,
as IPC calls, to whichever process registered as the console service.

## Layout

```
kernel/            the kernel
  main.rs          kmain, panic handler; denies unsafe code crate-wide
  arch/            hardware layer: the only place with unsafe Rust (besides sync.rs)
    boot.rs        Multiboot entry, 32->64-bit switch, boot page tables (asm)
    cpu.rs         privileged instructions; port I/O on a fixed list of devices
    entry.rs       exception/IRQ/syscall entry stubs, return to user mode (asm)
    context.rs     kernel stacks, context switch
    phys.rs        physical memory access, limited to allocator-owned frames
    multiboot.rs   boot-loader information: memory map, boot modules
    x86.rs         names for the CPU's numbers: register bits, vectors, descriptors
  sync.rs          Lock: a cell that panics on re-entrant use instead of aliasing
  gdt.rs           GDT + TSS (layout chosen for SYSCALL/SYSRET), I/O bitmap
  idt.rs           IDT, CPU exception handling
  irq.rs           8259 PIC, forwarding IRQs to drivers
  pmm.rs           Physical frame allocator (Multiboot memory map)
  vmm.rs           Paging: map, translate, clone (fork), destroy, user-memory copies
  elf.rs           ELF64 loader for user programs
  fs.rs            RAM file system (the unpacked ramdisk), open file descriptions
  process.rs       Process table, scheduler, fork/exit/wait, signals, mmap, IPC, services
  signal.rs        Dispositions, default actions, signal sets, the signal frame
  syscall.rs       Syscall dispatcher (Linux-compatible numbers)
  serial.rs        COM1 kernel debug log, print!/println!
abi/               syscall numbers, errno values, IPC protocols, the 16550 UART's
                   registers (kernel + user)
user/rt/           user runtime: entry point, syscall wrappers, print!, panics
user/console/      console server
user/proctest/     test probe for the process server (booted only by its tests)
user/init/         init (pid 1): /etc/motd, then a job control shell
  src/words.rs     command line words: quotes, `$?`, `&`, redirection operators
  src/redirect.rs  applying (and undoing) redirections
  src/traps.rs     the shell's signal handling and `trap`
user/user.ld       linker script for user programs
rootfs/            contents of the boot ramdisk (rootfs/etc/motd = /etc/motd)
test/              tests (see below)
```

### Unsafe code

Unsafe Rust is used only where nothing else can do the job: privileged
instructions, the entry and context-switch assembly, and memory that the
boot loader or the hardware defines. It lives in a few small modules that
wrap it in safe interfaces. The restrictions of those interfaces are what
make them safe:

* `kernel/arch/`: port I/O only on a fixed list of devices that cannot do
  DMA, with bounds-checked register offsets. Physical memory access only to
  frames the allocator owns; `phys` itself refuses anything in the kernel
  image, in boot-loader data or below 1 MiB. A context switch only to
  contexts that the context module itself saved. (On top of that,
  `vmm::map_user` maps only allocator frames, so user space never sees
  kernel memory.)
* `kernel/sync.rs`: `Lock`, which guards the process table and the TSS (all
  other shared kernel state is atomics). On this single CPU it can only be
  found held through re-entrant use, which panics instead of creating
  aliased `&mut`.
* `user/rt`: the `syscall` instruction and the start-up code. The wrappers
  pass the kernel only pointers taken from borrows of the right length and
  mutability (`&mut` whenever the kernel or an IPC server may write). A
  `write()` buffer can be a shared borrow because the kernel enforces that
  the console server only reads it (see below).
* `user/console`: its UART's port I/O (its own 8 registers).

Every other kernel module is checked by the compiler: `main.rs` sets
`#![deny(unsafe_code)]` and allows it only for `arch` and `sync`. `init` is
`#![forbid(unsafe_code)]`. Every unsafe block carries a `SAFETY:` comment
(`clippy::undocumented_unsafe_blocks` is denied). The kernel also relies on
compiler-builtins for `memcpy` and friends instead of defining them itself.

### Boot

The boot loader loads the kernel and a list of **boot modules**. Every module
that is an ELF file is started as a process, in order. The first becomes
pid 1 (init). Boot-module processes are *privileged*: they may use `ioperm`,
`irq_bind` and `svc_register`. Their forked children are not privileged.
A module that is not an ELF file is the **ramdisk**, a ustar archive: the
kernel unpacks it into its RAM file system (see *Files* below), so `/etc/motd`
is an ordinary file that init opens.

When init exits, the kernel powers the machine off.

### Virtual address space (per process)

```
PML4[0]       kernel: identity map of first 1 GiB (shared, supervisor-only)
PML4[1..256]  user (private):
  0x0000_0080_0000_0000  program image   (ELF segments: R-X / R-- / RW-)
  0x0000_0100_0000_0000  mmap() region   (RW-, NX unless PROT_EXEC)
  0x0000_7FFF_FFFF_F000  top of stack    (RW-, NX, 32 KiB, grows down)
```

User pages are mapped 4 KiB at a time, so `fork` is a deep copy (no COW yet).
W^X is enforced for the image/stack/mmap via the NX bit (EFER.NXE is on); the
ELF loader refuses segments that are both writable and executable.

### Syscall ABI

`syscall` instruction; `rax` = number; args in `rdi rsi rdx r10 r8 r9`;
return in `rax` (negative errno on failure). POSIX calls use **Linux x86_64 numbers**.
All numbers and constants live in `abi/src/lib.rs`.

| Nr     | Call                              | Notes                                                   |
|--------|-----------------------------------|---------------------------------------------------------|
| 0      | `read(fd,buf,len)`                | a file, or the terminal (one line per read)             |
| 1      | `write(fd,buf,len)`               | a file, or the terminal                                 |
| 2      | `open(path,flags,mode)`           | `O_CREAT`/`EXCL`/`TRUNC`/`APPEND`/`DIRECTORY`; no modes |
| 3      | `close(fd)`                       |                                                         |
| 9      | `mmap(_,len,prot,...)`            | anonymous only; kernel chooses the address              |
| 13     | `rt_sigaction(sig,act,old,sz)`    | Linux `struct sigaction`; a handler needs `SA_RESTORER` |
| 14     | `rt_sigprocmask(how,set,old,sz)`  | `SIG_BLOCK` / `SIG_UNBLOCK` / `SIG_SETMASK`             |
| 15     | `rt_sigreturn()`                  | from a handler's restorer; restores the saved context   |
| 24     | `sched_yield()`                   |                                                         |
| 32     | `dup(fd)`                         | lowest free descriptor; shares the open file description |
| 33     | `dup2(fd,newfd)`                  |                                                         |
| 34     | `pause()`                         | sleeps until a signal; `-EINTR` after a handler ran     |
| 39     | `getpid()`                        |                                                         |
| 57     | `fork()`                          | eager copy; child returns 0; privileges not inherited   |
| 60     | `exit(code)`                      |                                                         |
| 61     | `wait4(pid,&st,opts,..)`          | `pid`/-1/0/-pgid, `WNOHANG`; POSIX wait-status encoding |
| 62     | `kill(pid,sig)`                   | process, group (`pid` < 0) or existence check (`sig` 0) |
| 72     | `fcntl(fd,cmd,arg)`               | `F_DUPFD` only                                          |
| 109    | `setpgid(pid,pgid)`               | caller or a child; not a session leader                 |
| 110    | `getppid()`                       |                                                         |
| 112    | `setsid()`                        | new session and process group                           |
| 121    | `getpgid(pid)`                    |                                                         |
| 124    | `getsid(pid)`                     |                                                         |
| 173    | `ioperm(from,num,on)`             | privileged; ports `0..0x400` only                       |
| 217    | `getdents64(fd,buf,len)`          | Linux `struct linux_dirent64` records                   |
| 0x1000 | `ipc_send(pid,a,b)`               | async; non-blocking; `-EAGAIN` if mailbox (8) is full   |
| 0x1001 | `ipc_recv(&[u64;3])`              | blocking; `[sender, a, b]` (see below)                  |
| 0x1002 | `ipc_call(pid,op,buf,len)`        | synchronous; blocks until the server replies            |
| 0x1003 | `ipc_reply(client,status)`        | completes a received call; client gets `status`         |
| 0x1004 | `ipc_read(client,off,dst,len)`    | copy from the client's call buffer (if the call allows) |
| 0x1005 | `ipc_write(client,off,src,len)`   | copy into the client's call buffer (if the call allows) |
| 0x1006 | `irq_bind(line)`                  | privileged; route an IRQ line to the caller             |
| 0x1007 | `irq_ack(line)`                   | unmask the line after servicing the device              |
| 0x1008 | `svc_register(id)`                | privileged; become a well-known service (1 = console)   |
| 0x1009 | `svc_lookup(id)`                  | pid of a registered service                             |
| 0x100A | `debug_log(buf,len)`              | kernel debug log; never blocks (used for panics)        |
| 0x100B | `proc_list(buf,len)`              | one `ProcInfo` per process; `ps` is built on it         |
| 0x100C | `kill_pgrp(pgid,sig)`             | signal a process group, also group 1 (for the terminal) |

`ipc_recv` delivers, in order: IRQ notifications `[0, KMSG_IRQ, line]`
(sender 0 is the kernel and cannot be forged), mailbox messages, then the
oldest pending call `[client, op, len]`. A server may keep a call pending,
without replying, for as long as it likes. The console server does this for
`read` until a full line has been typed. If a server exits, its clients'
calls fail with `-EPIPE`.

While a call is pending, the server can only reach the client's buffer
through `ipc_read` / `ipc_write`, and the kernel limits even that to what the
call allows. An explicit `ipc_call` lets the server read and write the
buffer. For the terminal `write` and `read` calls it forwards, the kernel
lets the console server only *read* a `write()` buffer and only *write* a
`read()` buffer. Anything else fails with `-EACCES`, so not even a buggy or
malicious console server can modify memory that a writer only lent it, or
see what a reader's buffer held before.

### Files

The kernel keeps a small RAM file system, filled at boot from the boot
ramdisk and writable from then on, like Linux's initramfs: up to 64 files and
directories, of up to 64 KiB each, in page frames from the frame allocator.
Paths are normalised (`//`, `.` and `..`), and there is no current directory,
so a relative path starts at the root. Files can be created (in an existing
directory), written, appended to and truncated; there is no `unlink`,
`mkdir` or `lseek` yet.

File descriptors follow POSIX. Each process has 16, which refer to *open file
descriptions* (a file, an offset and an access mode) in a table of 64 shared
by the whole system. `dup`, `dup2` and `fork` make descriptors share a
description, and so its offset, which is what makes `cmd > f 2>&1` write the
output and the errors one after the other. Every process starts with
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

### Console server

`user/console` gets ports `0x3F8..0x3FF` via `ioperm`, binds IRQ 4 and
registers as the console service. It is a canonical-mode terminal: input is
echoed and can be edited (Backspace, Ctrl-U kills the line, Ctrl-C abandons
it, cursor-key escape sequences are ignored) until Enter. A `read` then
returns that line. Ctrl-D on an empty line is end-of-file. Output turns `\n`
into `\r\n`. The protocol (`OP_WRITE`, `OP_READ`, `OP_SET_FG_PGRP`) is in
`abi/src/lib.rs`.

It is also the *job control terminal*: it remembers a foreground process
group (`OP_SET_FG_PGRP`, the `tcsetpgrp()` of HaisuliOS), and on Ctrl-C it
abandons the line being typed and sends `SIGINT` to that group with
`kill_pgrp`. (Not `kill(-pgid)`: between jobs the foreground group is the
shell's, group 1, and `kill(-1)` is POSIX's broadcast to every process.) The
shell catches `SIGINT` with a signal handler, so its pending `read` fails with
`-EINTR` and it prompts again. The kernel withdraws an interrupted call, so the
server can no longer touch the reader's buffer; it just finds the reader gone.

### Signals and job control

Signals are a kernel mechanism; who gets them is user-space policy. Every
process has a process group and a session (inherited by `fork`, changed with
`setpgid` / `setsid`), a disposition for every signal and a mask of blocked
signals (both inherited by `fork`), and its pending signals.

A signal's *default action* happens as soon as it is sent, in the sender's
context: the target is terminated (its wait status is the signal number),
stopped or continued. POSIX's rules for a stopped process hold: only
`SIGKILL` and `SIGCONT` act on it, anything else stays pending until it is
continued, and `SIGCONT` discards pending stop signals. Terminating a process
wakes its parent out of `wait4`, fails the calls it was serving with `-EPIPE`,
and reparents its children to init. A blocked signal waits until it is
unblocked.

A *caught* signal runs a handler when its process next returns to user mode.
The scheduler is cooperative, so a process only stops running inside a system
call, and the end of every system call is the one place where this happens.
There the kernel pushes a signal frame on the user stack and enters the
handler with the signal number, a `siginfo_t` and a `ucontext_t`, blocking
the signal (unless `SA_NODEFER`) and `sa_mask` meanwhile. The frame has the
Linux x86_64 layout, so a libc ported later finds things where it expects
them. The handler returns to its `SA_RESTORER`, which calls `rt_sigreturn` to
restore the interrupted registers and signal mask. The kernel treats that
context as untrusted: it keeps the user code and stack segments, only takes
the arithmetic flags from it, and a frame that cannot be read or written, or
that would resume at a kernel address, kills the process with `SIGSEGV`.

A process asleep in `pause`, `wait4`, `ipc_recv` or a console `read` is
woken by a caught signal, and the call fails with `-EINTR`, or is restarted
after the handler with `SA_RESTART` (except `pause`, as POSIX requires).
Console writes and other IPC calls always complete. `SA_RESETHAND` makes a
handler one-shot.

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
haisuli# pause &          a job of its own, in process group 3
[1] 3
haisuli# kill -STOP 3     ps then shows it as T (stopped)
haisuli# kill -TERM 3     pending: a stopped process is not terminated yet
haisuli# kill -CONT 3     continued, and the pending SIGTERM arrives
haisuli# wait 3
haisuli# echo $?
143                       128 + SIGTERM, as POSIX requires
haisuli# trap 'echo caught USR1' USR1
haisuli# kill -USR1 $$    the shell's handler runs, then the trap command
caught USR1
```

## Build & run

Requirements: Rust (stable, **via `rustup`**), `make`, `tar`, `objcopy`
(binutils; on macOS use `OBJCOPY=x86_64-elf-objcopy` or `llvm-objcopy`), and
`qemu-system-x86_64`. A distribution-packaged Rust (e.g. `apt install cargo`)
usually lacks the `x86_64-unknown-none` target; rustup installs it
automatically from `rust-toolchain.toml`.

```sh
make          # build kernel + user programs, pack rootfs/ into build/initrd.tar
make run      # boot in QEMU with an interactive console on your terminal
```

In `make run`, keys (including Ctrl-C) go to HaisuliOS. Type `exit` or press
Ctrl-D at the prompt to power off, or press **Ctrl-A X** to quit QEMU.

`rust-toolchain.toml` pulls the `x86_64-unknown-none` target automatically and
the Makefile builds for it (`--target x86_64-unknown-none`). Each image's
`build.rs` applies its linker script and links it as a static executable.
`.cargo/config.toml` only makes plain `cargo build` / `cargo clippy` default to
the bare-metal target too; outputs land in `target/x86_64-unknown-none/`.

Why the `objcopy` step? QEMU's `-kernel` loader only accepts 32-bit ELF files;
the kernel itself switches to long mode in `kernel/arch/boot.rs`.
QEMU passes the boot modules with `-initrd "build/init,build/console,build/initrd.tar"`.
With GRUB you can boot the 64-bit ELF directly:

```
multiboot /boot/haisulios
module    /boot/init init
module    /boot/console console
module    /boot/initrd.tar initrd
```

### Troubleshooting

* **`linking with cc failed` ... `multiple definition of _start` ... `Scrt1.o`**
  (paths under `target/release/` rather than `target/x86_64-unknown-none/`):
  cargo built for your host OS instead of bare metal. Use `make`, or pass
  `--target x86_64-unknown-none` to cargo yourself. If you copied the tree by
  hand, check that hidden files (`.cargo/`, `.gitignore`) came along.
* **``can't find crate for `core` ``**: the `x86_64-unknown-none` target is
  not installed. Use rustup (`rustup target add x86_64-unknown-none`).

### Expected output

```
HaisuliOS 0.1.0 - x86_64 microkernel
[pmm] 32364 frames free (126 MiB)
[kernel] started pid 1 (init): 30 KiB image
[kernel] started pid 2 (console): 26 KiB image
[kernel] ramdisk "initrd.tar": 10 KiB
[kernel] entering user space
[console] serving COM1 (IRQ 4)

  _   _       _           _ _  ___  ____
 ...                                          (the contents of /etc/motd)

haisuli# help
commands:
  help              this list
  echo [text...]    print text
  ls [dir]          list a directory
  cat [file...]     print files (or the input)
  ps                list processes
  pause             wait for a signal (as a job: pause &)
  kill [-sig] pid   signal a process (-pid: a process group)
  wait [pid...]     wait for background jobs
  trap [cmd sig...] run cmd on a signal ('' ignores, - resets)
  exit [status]     leave the shell and power off
end a command with & to run it in the background; $? is its status
redirect with > file, >> file, < file, 2> file and 2>&1
haisuli# pause &
[1] 3
haisuli# ps
  PID  PPID  PGID   SID STAT NAME
    1     0     1     1 R    init
    2     0     2     2 S    console
    3     1     3     1 S    init
haisuli# kill 3
[kernel] pid 3 (init) exited (wait status 0xf)
haisuli# wait 3
haisuli# echo $?
143
haisuli# exit
[kernel] pid 1 (init) exited (wait status 0x0)
[kernel] init exited - shutting down
```

QEMU then exits via `isa-debug-exit` (host exit status 1 is expected).

## Tests

```sh
make check                          # build, then run every test
make check TEST_ARGS="echo"         # only tests whose id contains "echo"
make check TEST_ARGS="-v -j 1"      # one at a time, print every transcript
make check TEST_EXPLICIT="process-server"  # only the explicit target process-server
python3 test/run_tests.py --list    # list the test ids
```

Tests for work in progress form *explicit targets*: a test file sets
`EXPLICIT = "name"`, and its tests, which are expected to fail until the
feature exists, never run in a plain `make check`. `TEST_EXPLICIT` runs them,
and only them; its name must match the target exactly (`TEST_ARGS` selectors
match parts of test ids, but never pick up an explicit target). Selectors in
`TEST_ARGS` still narrow the target down. A test file can also boot extra
programs after the usual boot modules (`EXTRA_MODULES`).

| Explicit target  | Tests                           | Describes                                  |
|------------------|---------------------------------|--------------------------------------------|
| `process-server` | `test/test_process_server.py`   | `fork`, `exit`, `wait4`, `getpid`, `getppid` in a user-space process server instead of the kernel |

The tests drive the shell the way a user would. Each test boots its own
QEMU machine with the console on a pipe, types at the prompt, and checks what
appears on the screen. Tests run in parallel, and only the Python 3 standard
library is needed. After every test, the transcript is also checked for
kernel panics, CPU exceptions and user-space panics. On failure, the runner
prints the traceback and the full console transcript.

```
test/run_tests.py         runner: discovery, parallel execution, reporting
test/haisuli.py           harness: Machine (boot, send, expect, run, run_raw)
test/test_boot.py         boot messages, /etc/motd shown before the first prompt
test/test_commands.py     help, echo, ls, cat, unknown commands, odd input
test/test_line_editing.py echo, Backspace, Ctrl-U/C, escape sequences, long lines
test/test_exit.py         exit and Ctrl-D power the machine off
test/test_process.py      signals, process groups/sessions, waitpid semantics
test/test_shell_redirection.py  >, >>, <, 2> and 2>&1, backed by the kernel's files
test/test_process_server.py     explicit target process-server (see above)
```

A test is a function `test_*(sh)` in a `test/test_*.py` file. It receives a
machine waiting at its first prompt: `sh.run("echo hi")` types a command and
returns its output (`"hi\n"`), `sh.run_raw(keys)` also returns the echoed
line, and `sh.send()` / `sh.expect()` give full control.

## Coding style

The code follows the Linux kernel's Rust coding guidelines
(`Documentation/rust/coding-guidelines.rst`):

* Formatting is `rustfmt` with default settings (`.rustfmt.toml`: 4-space
  indentation, 100 columns). Run `make rustfmt` to format and
  `make rustfmtcheck` to check.
* Comments and docs are Markdown, start with a capital letter and end with a
  period, including `// SAFETY:` comments. The first paragraph of every doc
  comment is a single sentence. Functions that can panic have a `# Panics`
  section, and unsafe functions have a `# Safety` section.
* No magic numbers. A hardware or format constant has a name and a doc
  comment saying what it is: an enum for a set of alternatives or the flags of
  one register (`Selector`, `Exception`, `RFlag`, `PageFlag`, the UART's
  `LineStatus`, ...), with `bits(&[...])` for the flags that get combined; a
  documented constant for a size or count. The assembly takes them as `const`
  operands rather than repeating the numbers.
* Every workspace crate uses the kernel's Rust lint set (`rust_common_flags`
  in the kernel Makefile), configured under `[workspace.lints]` in
  `Cargo.toml`: for example `unreachable_pub`, `rust_2018_idioms`,
  `missing_docs` and the main clippy groups as errors. Lint exceptions use
  `#[expect]` rather than `#[allow]`.
* Every file starts with an SPDX license identifier, as described in the
  kernel's `Documentation/process/license-rules.rst`. Rust source files
  follow it with a module header:

  ```rust
  // SPDX-License-Identifier: GPL-2.0

  //! Module name.
  //!
  //! One sentence describing what the module accomplishes.
  ```

The Python tests follow PEP 8 with the same 100-column limit.

## License

HaisuliOS is licensed under the GNU General Public License, version 2 only
(`SPDX-License-Identifier: GPL-2.0`). See `COPYING` for the full text.

## Known limitations (by design for v0.1)

* Cooperative scheduling only: IRQs wake drivers but do not preempt, and the
  timer is not used yet. The CPU halts when nothing is runnable.
* Identity-mapped kernel in the low 1 GiB (not higher-half); RAM above 1 GiB unused.
* Single CPU, no locking. Kernel stacks are static (max 16 processes).
* No `exec`: programs can only be started as boot modules. Boot-module
  memory is not reclaimed after the programs are loaded.
* The kernel debug log and the console server share COM1 (output only on
  the kernel's side), so kernel messages can interleave with console output.
* Signal handlers run only when a process returns from a system call. With
  cooperative scheduling that is always before it runs again, except for a
  forked child that has not run yet: its signals wait for its first system
  call. CPU exceptions are not signals to a handler: a fault still
  terminates the process (`SIGSEGV`). There is no `sigaltstack`, and
  `SA_SIGINFO` changes nothing (every handler gets all three arguments).
* The shell has single quotes and redirections, but no double quotes, `;`
  or pipes, and a redirection operator must start a word (`echo a >b`, not
  `echo a>b`).
* A zombie whose parent died is reparented to init, but init only reaps the
  children it knows about, so such a zombie keeps its slot in the process
  table.
* Stopping a server that the rest of the system depends on (for instance
  `kill -STOP 2`, the console) leaves nothing runnable; the kernel reports
  the deadlock and powers off.
* Syscalls validate every user pointer against the page tables, but
  "privileged" (boot module or not) is the only permission model; there are
  no capabilities yet, and no file permissions either.

## Roadmap toward POSIX

1. Timer (LAPIC/PIT) + preemption. (Signal handlers are entered at the end of
   system calls, which is enough only while scheduling is cooperative; with
   preemption, returns from interrupts need the same `deliver_signals` step.)
2. Higher-half kernel, kernel heap, copy-on-write `fork`.
3. `execve` (the ELF loader exists), loading programs from the file system.
4. Capability-based IPC endpoints; shared memory / page grants.
5. More user-space servers: a VFS, with the RAM file system moving out of the
   kernel into a file system server; then port musl libc.
6. Move the fd table out of the kernel (libc/VFS), so that `read`/`write`
   reach servers directly rather than via the kernel.
