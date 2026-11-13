# HaisuliOS

A tiny, bare-bones **microkernel** for **x86_64**, written in **Rust**, with a
**POSIX-flavoured** syscall ABI. No external crates, no bootloader crate, no
nightly features: just `core`, inline assembly, and a Multiboot v1 header.

> Status: v0.1. It provides virtual memory, processes with POSIX signals,
> process groups, sessions and `waitpid` semantics, IPC and user-space
> drivers, and boots into a job control shell on a console served by a
> user-space console server. It is **not** yet a POSIX system (no `exec`,
> no files).

## What the kernel does (and deliberately doesn't)

| In the kernel (mechanism)                             | In user space (policy)                      |
|-------------------------------------------------------|---------------------------------------------|
| Physical memory manager (bitmap frame allocator)      | **Console server**: COM1 driver + line discipline + controlling terminal (`user/console`) |
| Virtual memory: 4-level paging, per-process spaces    | **init**: motd + a job control shell (`user/init`) |
| Processes, kernel stacks, context switch, scheduler   | Job control: foreground process group, Ctrl-C -> `SIGINT` (console server) |
| `fork`, `exit`, `wait4`, `getpid`, `mmap` (anon)      | Jobs, `kill`, `wait`, `ps`, `$?` (init)     |
| POSIX signals, process groups, sessions (`process/`)  | Planned: VFS + file system servers          |
| IPC: async notifications + synchronous calls with data transfer | Planned: more device drivers      |
| IRQ forwarding, per-process I/O-port permissions      | Planned: libc (musl port) on the ABI        |
| ELF loader for boot modules; well-known service registry |                                          |
| Exceptions -> fault handling (SIGSEGV-style kill)     |                                             |

The kernel contains no device drivers apart from a write-only debug log on
COM1. `read(0)`, `write(1|2)` and the terminal `ioctl`s keep their Linux
syscall numbers, but the kernel only forwards them, as IPC calls, to
whichever process registered as the console service.

Signals, process groups and sessions are kernel mechanisms because they need
the scheduler and the user registers: stopping a process, interrupting a
blocked call with `EINTR`, running a handler. What the keyboard means
(Ctrl-C is `SIGINT` for the foreground process group) and what a job is are
policy, in the console server and the shell. See [Processes and signals](#processes-and-signals).

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
  sync.rs          Lock: a cell that panics on re-entrant use instead of aliasing
  gdt.rs           GDT + TSS (layout chosen for SYSCALL/SYSRET), I/O bitmap
  idt.rs           IDT, CPU exception handling
  irq.rs           8259 PIC, forwarding IRQs to drivers
  pmm.rs           Physical frame allocator (Multiboot memory map)
  vmm.rs           Paging: map, translate, clone (fork), destroy, user-memory copies
  elf.rs           ELF64 loader for user programs
  process.rs       Process table, scheduler, fork/exit/wait, mmap, IPC, services
  process/
    signal.rs      signals: generation, delivery, handler frames, kill, sigaction
    group.rs       process groups, sessions, process names, process listing (ps)
  syscall.rs       Syscall dispatcher (Linux-compatible numbers)
  serial.rs        COM1 kernel debug log, print!/println!
abi/               syscall numbers, errno values, IPC protocols (kernel + user)
user/rt/           user runtime: entry point, syscall wrappers, print!, panics
user/console/      console server
user/init/         init (pid 1): /etc/motd, then a job control shell (jobs.rs, signals.rs)
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
A module that is not an ELF file is the **ramdisk**, a ustar archive that is
mapped read-only into init (address and size in `rdi`/`rsi`). There is no file
system server yet, so init reads `/etc/motd` from the archive itself.

When init exits, the kernel powers the machine off.

### Virtual address space (per process)

```
PML4[0]       kernel: identity map of first 1 GiB (shared, supervisor-only)
PML4[1..256]  user (private):
  0x0000_0080_0000_0000  program image   (ELF segments: R-X / R-- / RW-)
  0x0000_0100_0000_0000  mmap() region   (RW-, NX unless PROT_EXEC)
  0x0000_7000_0000_0000  boot ramdisk    (R--, init only)
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
| 0      | `read(0,buf,len)`                 | forwarded to the console server (one line per read)     |
| 1      | `write(fd,buf,len)`               | fd 1/2, forwarded to the console server                 |
| 9      | `mmap(_,len,prot,...)`            | anonymous only; kernel chooses the address              |
| 13     | `rt_sigaction(sig,act,old,8)`     | Linux `struct kernel_sigaction`; `SA_RESTART` etc.      |
| 14     | `rt_sigprocmask(how,set,old,8)`   | `SIGKILL`/`SIGSTOP` cannot be blocked                   |
| 15     | `rt_sigreturn()`                  | only for the handler's restorer                         |
| 16     | `ioctl(fd,req,arg)`               | fd 0-2: `TIOCGPGRP`/`TIOCSPGRP`, forwarded to the console |
| 24     | `sched_yield()`                   |                                                         |
| 34     | `pause()`                         | until a handler has run; always `-EINTR`                |
| 39     | `getpid()`                        |                                                         |
| 57     | `fork()`                          | eager copy; child returns 0; privileges not inherited   |
| 60     | `exit(code)`                      |                                                         |
| 61     | `wait4(pid,&st,opt,_)`            | pid/-1/0/-pgid; `WNOHANG WUNTRACED WCONTINUED`          |
| 62     | `kill(pid,sig)`                   | pid/0/-1/-pgid; sig 0 only checks                       |
| 109    | `setpgid(pid,pgid)`               | caller or a child in its session                        |
| 110    | `getppid()`                       |                                                         |
| 111    | `getpgrp()`                       |                                                         |
| 112    | `setsid()`                        | `-EPERM` for a process group leader                     |
| 121    | `getpgid(pid)`                    |                                                         |
| 124    | `getsid(pid)`                     |                                                         |
| 157    | `prctl(PR_SET_NAME/PR_GET_NAME,name)` | the name `ps` and the kernel log show               |
| 173    | `ioperm(from,num,on)`             | privileged; ports `0..0x400` only                       |
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
| 0x100B | `proc_info(pid,buf)`              | the process with the lowest pid >= `pid` (for `ps`)     |
| 0x100C | `killpg(pgrp,sig)`                | like `kill(-pgrp)`, but also for process group 1         |

`ipc_recv` delivers, in order: IRQ notifications `[0, KMSG_IRQ, line]`
(sender 0 is the kernel and cannot be forged), mailbox messages, then the
oldest pending call `[client, op, len]`. A server may keep a call pending,
without replying, for as long as it likes. The console server does this for
`read` until a full line has been typed. If a server exits, its clients'
calls fail with `-EPIPE`. If a signal interrupts a client, its call is
cancelled and fails with `-EINTR`: the server can no longer reply to it or
transfer data.

While a call is pending, the server can only reach the client's buffer
through `ipc_read` / `ipc_write`, and the kernel limits even that to what the
call allows. An explicit `ipc_call` lets the server read and write the
buffer. For the `write(1|2)` and `read(0)` calls it forwards, the kernel lets
the console server only *read* a `write()` buffer and only *write* a
`read()` buffer. Anything else fails with `-EACCES`, so not even a buggy or
malicious console server can modify memory that a writer only lent it, or
see what a reader's buffer held before.

### Console server

`user/console` gets ports `0x3F8..0x3FF` via `ioperm`, binds IRQ 4 and
registers as the console service. It is a canonical-mode terminal: input is
echoed and can be edited (Backspace, Ctrl-U kills the line, cursor-key escape
sequences are ignored) until Enter. A `read` then returns that line. Ctrl-D on
an empty line is end-of-file. Output turns `\n` into `\r\n`.

It is also the controlling terminal. `tcsetpgrp` (`ioctl(TIOCSPGRP)`, which
the kernel forwards as `OP_SETPGRP`) sets the foreground process group; the
first session to do so (init's) becomes the terminal's session, and the
server checks that the caller and the group belong to it, using `proc_info`.
Ctrl-C and Ctrl-\ send `SIGINT` and `SIGQUIT` to the foreground process
group and, as on a POSIX terminal, discard the line being edited and any
input not read yet. The protocol (`OP_WRITE`, `OP_READ`, `OP_SETPGRP`,
`OP_GETPGRP`) is in `abi/src/lib.rs`.

### Processes and signals

Processes have POSIX process groups and sessions. Every boot module leads its
own session and group; a child starts in its parent's. Pids are handed out in
increasing order (wrapping at 32768), so a pid is not reused right after its
process was reaped, and never while it is in use as a process group or
session id. When a process exits, its children are handed to init.

`wait4` waits for one child, any child, or any child in a process group, and
reports children that exited, and with `WUNTRACED` / `WCONTINUED` also those
that stopped or continued. A parent that ignores `SIGCHLD` (or sets
`SA_NOCLDWAIT`) does not get zombies.

Signals 1 to 31 have their Linux numbers and default actions. A signal is
*generated* by `kill`/`killpg` (or the kernel: `SIGCHLD`), and generation
already does what cannot wait:

* `SIGKILL` and `SIGCONT` resume a stopped process; `SIGCONT` discards
  pending stop signals and a stop signal discards a pending `SIGCONT`.
* A stop signal with the default action stops the process at once. A stopped
  process is simply not scheduled; it stays in whatever call it was blocked
  in. So a stopped `pause()` is still in `pause()` after `SIGCONT`, and a
  `SIGTERM` for a stopped process stays pending until it continues.
* A signal that would be ignored is discarded (unless it is blocked).
* Otherwise the signal is pending, and, unless blocked, interrupts any
  blocking call (`read`, `wait4`, `pause`, `ipc_recv`, `ipc_call`), which
  fails with `-EINTR`.

Pending signals are *delivered* at the end of every system call: the default
action (terminate, with the signal number as the wait status; stop;
continue; ignore) or a handler. For a handler, the kernel pushes a frame with
the user registers and the blocked signals onto the user stack (below the
red zone) and enters the handler as `handler(sig)`. The handler returns to the
restorer given in `rt_sigaction` (the runtime's calls `rt_sigreturn`), which
restores everything; from that untrusted frame the kernel takes only user
addresses and the arithmetic flags. With `SA_RESTART`, an interrupted call
(other than `pause`) is restarted after the handler.

There are no users yet: a privileged process (a boot module) may signal any
process, an ordinary one any but the privileged ones. As on Linux, init only
gets the signals it has a handler for.

The shell uses all of this for job control: each job runs in a process group
of its own, a foreground job gets the terminal (so Ctrl-C interrupts it, not
the shell), and Ctrl-C at the prompt only discards the line.

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
[pmm] 32339 frames free (126 MiB)
[kernel] started pid 1 (init): 59 KiB image + ramdisk
[kernel] started pid 2 (console): 27 KiB image
[kernel] ramdisk "initrd.tar": 10 KiB
[kernel] entering user space
[console] serving COM1 (IRQ 4)

  _   _       _           _ _  ___  ____
 ...                                          (the contents of /etc/motd)

haisuli# help
commands:
  help                this list
  echo [text...]      print text
  ls [dir]            list files on the boot ramdisk
  cat file...         print files
  ps                  list processes
  pause               wait for a signal (Ctrl-C interrupts it)
  kill [-sig] pid...  send a signal (default TERM; -l lists them)
  wait [pid...]       wait for background jobs
  exit [n]            leave the shell and power off
'command &' runs a command in the background. $? is the exit status of the
last command, $! the pid of the last background job and $$ the shell's pid.
haisuli# pause &
[1] 3
haisuli# ps
  PID  PPID  PGID   SID STAT COMMAND
    1     0     1     1 R    init
    2     0     2     2 S    console
    3     1     3     1 S    pause
haisuli# kill $!
haisuli# [kernel] pid 3 (pause) exited (wait status 0xf)
wait $!
haisuli# echo $?
143
haisuli# pause
^C
[kernel] pid 4 (pause) exited (wait status 0x2)
haisuli# echo $?
130
haisuli# exit
[kernel] pid 1 (init) exited (wait status 0x0)
[kernel] init exited - shutting down
```

The kernel logs the exit of every process on the shared COM1, so such lines
can appear anywhere, e.g. right after a prompt. QEMU then exits via
`isa-debug-exit` (host exit status 1 is expected).

## Tests

```sh
make check                          # build, then run every test
make check TEST_ARGS="echo"         # only tests whose id contains "echo"
make check TEST_ARGS="-v -j 1"      # one at a time, print every transcript
python3 test/run_tests.py --list    # list the test ids
```

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
test/test_process.py      signals, process groups/sessions, waitpid semantics, ps,
                          Ctrl-C and jobs (make check TEST_ARGS="process")
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
* Signals are delivered when a system call returns, which with cooperative
  scheduling is whenever a process could be interrupted at all. There are no
  real-time signals, no `sigaltstack`, and CPU exceptions terminate the
  process directly (a `SIGSEGV` handler is not called). Signal frames hold no
  FPU/SSE state, which user programs (built soft-float) do not use.
* Job control is partial: no Ctrl-Z, `fg` or `bg`; background jobs that use
  the terminal get no `SIGTTIN`/`SIGTTOU`; orphaned process groups get no
  `SIGHUP`/`SIGCONT`.
* Syscalls validate every user pointer against the page tables, but
  "privileged" (boot module or not) is the only permission model; there are
  no users or capabilities yet.

## Roadmap toward POSIX

1. Timer (LAPIC/PIT) + preemption.
2. Higher-half kernel, kernel heap, copy-on-write `fork`.
3. `execve` (the ELF loader exists); a file system server serving the ramdisk.
4. Capability-based IPC endpoints; shared memory / page grants.
5. More user-space servers: VFS + tmpfs, then port musl libc (signals,
   pgid/sid and `waitpid` semantics are done, see
   [Processes and signals](#processes-and-signals)).
6. Move the fd table out of the kernel (libc/VFS), so that `read`/`write`
   reach servers directly rather than via the kernel's console forwarding.
