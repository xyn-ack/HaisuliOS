# HaisuliOS

A tiny, bare-bones **microkernel** for **x86_64** (a PC) and **arm64** (QEMU's
`virt` machine), written in **Rust**, with a **POSIX-flavoured** syscall ABI.
No external crates, no bootloader crate, no nightly features: just `core`,
inline assembly, and a Multiboot v1 header or a Linux arm64 Image header.

> Status: v0.3.1. It provides virtual memory, tasks with any number of threads,
> a preemptive scheduler on every CPU of the machine (SMP), IPC and user-space
> drivers; POSIX processes, `execve` and signals with job control are
> implemented by a user-space process server, which loads programs from the file
> system, and the system boots into an interactive console served by a
> user-space console server. It is **not** yet a full POSIX system: there is no
> libc, and the file system is a RAM disk.

More documents:

* [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how it works inside: the source
  layout, boot, CPUs and threads, the system calls, processes, programs, files,
  the console and signals.
* [docs/TESTING.md](docs/TESTING.md): how it is tested.
* [CHANGELOG.md](CHANGELOG.md): what changed in each version.
* [TODO.md](TODO.md): the goals and the roadmap.

## What the kernel does (and deliberately doesn't)

| In the kernel (mechanism)                             | In user space (policy)                      |
|-------------------------------------------------------|---------------------------------------------|
| Physical memory manager (bitmap frame allocator)      | **Process server**: POSIX processes (`user/proc`): pids, `fork`, `execve`, `exit`, `wait4`, zombies, `getpid`, process groups and sessions, signals, and what threads mean for all of them; starts the system servers listed in `/etc/servers`, and restarts one that dies |
| Virtual memory: 4-level paging, per-task spaces       | **Console server**: COM1 driver, line discipline and the job control terminal (`user/console`, run from `/sbin/console`) |
| Tasks and their threads (`clone`, `exit`, `gettid`), kernel stacks, context switch | **Runtime** (`user/rt`): threads (`thread::spawn`, `join`), `nprocs` |
| Every CPU (SMP): started at boot, per-CPU state, APIC timer, IPIs; one preemptive scheduler for all | **init**: motd, then a job control shell (`user/init`) |
| Task control for the task manager: copy, end, stop, continue, upcalls | Job control policy: who is in the foreground, what Ctrl-C means |
| `mmap` (anonymous)                                    | Shell redirections (`>`, `>>`, `<`, `2>&1`) |
| RAM file system (the unpacked ramdisk), file descriptors | Programs in `/bin` (`user/coreutils`, and the test programs `user/probe_test` and `user/smp_test`) |
| IPC: async notifications + synchronous calls with data transfer |                                   |
| IRQ forwarding, device grants to drivers (I/O ports)  |                                             |
| ELF loader: boot modules, and programs from the file system (argv/envp on the stack) |             |
| Well-known service registry                           |                                             |
| Exceptions -> end the task, report it to the task manager |                                         |

The kernel contains no device drivers apart from a write-only debug log on
COM1 (which the console server also sends its output through), and it knows
nothing about POSIX processes: it runs *tasks* (an address space with one or
more threads), on all the CPUs, and the process server builds processes, with
their pids, parents, exit statuses and signals, on top of them (see
[Processes](docs/ARCHITECTURE.md#processes) and
[CPUs and threads](docs/ARCHITECTURE.md#cpus-and-threads)). A task's file descriptors refer
to files in the kernel's RAM file system or to the terminal; `read` and
`write` on the terminal are forwarded, as IPC calls, to whichever task
registered as the console service.

Next to move out of the kernel are the file system and the file descriptors,
behind a VFS; a libc (a musl port) and more drivers are planned too (see
[TODO.md](TODO.md)).

## Layout

```
kernel/           the kernel: boot, CPUs, memory, tasks and threads, IPC, the RAM file system
abi/              system call numbers (per architecture), errno values, IPC protocols and
                  device descriptions (kernel + user)
user/rt/          user runtime: entry point, system calls, POSIX process calls, threads, print!,
                  device access; all of user space's assembly, in src/arch/
user/proc/        process server: POSIX processes, signals and execve
user/console/     console server (/sbin/console)
user/init/        init (pid 1): /etc/motd, then a job control shell
user/coreutils/   programs for /bin: echo, true, false, nproc
user/probe_test/  /bin/probe_test, a test program
user/smp_test/    /bin/smp_test, a test program for multiple CPUs and threads
rootfs/           contents of the boot ramdisk, besides the programs the build adds
test/             tests (see docs/TESTING.md)
docs/             architecture and testing documentation
```

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#source-layout) has a file-by-file
map.

## Build & run

Requirements: Rust (stable, **via `rustup`**), `make`, `tar`, `objcopy`
(binutils; on macOS use `OBJCOPY=x86_64-elf-objcopy` or `llvm-objcopy`), and
`qemu-system-x86_64`; for arm64, `qemu-system-aarch64` (Debian and Ubuntu:
`qemu-system-arm`) and rustup's `llvm-tools` component (for `llvm-objcopy`:
GNU objcopy does not read arm64 ELF files). A distribution-packaged Rust (e.g.
`apt install cargo`) usually lacks the `x86_64-unknown-none` target; rustup
installs it automatically from `rust-toolchain.toml`.

```sh
make          # build kernel + user programs, pack rootfs/ and the programs
              # for /sbin and /bin into build/initrd.tar
make run      # boot in QEMU with an interactive console on your terminal
make run CPUS=4   # the same, on four CPUs
make ARCH=aarch64 run   # the same for arm64, on QEMU's virt machine; build, check and
                        # every other target take ARCH too (outputs in build/aarch64/)
make portcheck    # check that the system stays portable: the user programs build for
                  # aarch64-unknown-none (see below), and more
```

In `make run`, keys (including Ctrl-C) go to HaisuliOS. Type `exit` or press
Ctrl-D at the prompt to power off, or press **Ctrl-A X** to quit QEMU.

`rust-toolchain.toml` pulls the `x86_64-unknown-none` and
`aarch64-unknown-none` targets and `llvm-tools` automatically, and the Makefile
builds for `$(ARCH)-unknown-none`. If the stable release that rustup picks does
not ship the arm64 target (some do not; rustup then warns that it skips it),
the Makefile falls back to the 1.98.0 toolchain for arm64 (`rustup toolchain
install 1.98.0 --target aarch64-unknown-none --component llvm-tools`), or to
whichever you name: `AARCH64_TOOLCHAIN=+<toolchain>`. Each image's
`build.rs` applies its linker script and links it as a static executable.
`.cargo/config.toml` only makes plain `cargo build` / `cargo clippy` default to
the bare-metal target too; outputs land in `target/x86_64-unknown-none/`.

Why the `objcopy` step? QEMU's `-kernel` loader only accepts 32-bit ELF files;
the kernel itself switches to long mode in `kernel/arch/x86_64/boot.rs`.
QEMU passes the boot modules with `-initrd "build/init,build/proc,build/initrd.tar"`.
With GRUB you can boot the 64-bit ELF directly:

```
multiboot /boot/haisulios
module    /boot/init init
module    /boot/proc proc
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
HaisuliOS 0.3.1 - x86_64 microkernel
[pmm] 32364 frames free (126 MiB)
[kernel] started task 1 (init): 71 KiB image
[kernel] started task 2 (proc): 44 KiB image
[kernel] ramdisk "initrd.tar": 120 KiB
[kernel] 1 CPU online
[kernel] entering user space
[proc] started pid 3 (console) from /sbin/console
[proc] serving POSIX processes
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
  program [args...] run a program in /bin (or give its path)
end a command with & to run it in the background; $? is its status
redirect with > file, >> file, < file, 2> file and 2>&1
haisuli# pause &
[1] 4
haisuli# ps
  PID  PPID  PGID   SID STAT THR NAME
    1     0     1     1 R      1 init
    2     0     2     2 R      1 proc
    3     0     3     3 S      1 console
    4     1     4     1 S      1 init
haisuli# kill 4
[proc] pid 4 (init) exited (wait status 0xf)
haisuli# wait 4
haisuli# echo $?
143
haisuli# exit
[proc] pid 1 (init) exited (wait status 0x0)
[proc] init exited - shutting down
[kernel] powering off
```

QEMU then exits via `isa-debug-exit` (host exit status 1 is expected).

`make run` gives the machine one CPU; `make run CPUS=4` gives it four (up to
16). The kernel then says `[kernel] 4 CPUs online`, and `nproc` prints 4.

## Tests

```sh
make check                          # build, then run every test
make check TEST_ARGS="echo"         # only tests whose id contains "echo"
make check TEST_EXPLICIT="process-server"  # only the explicit tests named process-server
make check TEST_EXPLICIT="smp"      # the SMP and thread tests, on machines with 1 to 4 CPUs
make check TEST_ARGS="--cpus 4"     # every other test, on machines with 4 CPUs
```

Each test boots its own QEMU machine and drives the shell over the serial
console. [docs/TESTING.md](docs/TESTING.md) explains the options, the explicit
tests, the test program `/bin/probe_test` and how to write a test.

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

## Known limitations

By design for v0.3; [TODO.md](TODO.md) has the plans for them.

* Identity-mapped kernel in the low 1 GiB (not higher-half); RAM above 1 GiB unused.
* Up to 16 CPUs, found in the ACPI tables (without them, only the boot CPU
  runs). One scheduler lock for all of them, and one global run queue; device
  IRQs go to the boot CPU only (through the 8259 PICs, no I/O APIC). The timer
  only preempts: there is no `sleep`, and no other timer for user space.
* Kernel stacks are static: at most 64 threads and 16 tasks at once. A zombie
  takes no task, only a record in the process server, which keeps up to 64
  processes, zombies included.
* Threads: no futex yet, so `join` waits by yielding; no thread-local storage
  (`CLONE_SETTLS`); no `munmap`, so a thread stack is reused by later threads
  of the process rather than freed. Signal masks and dispositions belong to the
  process, not to each thread, and a signal goes to a thread in `pause` or
  `wait4` if there is one, otherwise to one the kernel picks. A device granted
  by one thread becomes usable by the other threads of its task only the next
  time they are switched to.
* Programs are static ELF executables of up to 128 KiB (the file system's
  limit), with at most 4 KiB of arguments and environment. There is no
  `PATH`, no close-on-exec and no `#!` scripts. Boot-module memory is not
  reclaimed after the programs are loaded.
* The kernel debug log and the console server share COM1. Each message, and
  each piece of terminal output, goes out in one piece (the console server
  sends its output through the kernel's transmitter), but log lines can still
  appear between two pieces of a program's output.
* Signal handlers run when a thread returns to user mode: at the end of a
  system call, or of an interrupt (so a busy thread on another CPU enters its
  handler at once: the kernel interrupts its CPU). CPU exceptions are not
  signals to a handler: the kernel ends the task and the process dies of
  `SIGSEGV`. There is no `sigaltstack`, and
  `SA_SIGINFO` changes nothing (every handler gets all three arguments). No
  `SIGCHLD` is sent.
* The process server is trusted and nothing restarts it: if it died, every
  POSIX process call would fail. It cannot be signalled.
* The shell has single quotes and redirections, but no double quotes, `;`
  or pipes, and a redirection operator must start a word (`echo a >b`, not
  `echo a>b`).
* A zombie whose parent died is reparented to init, but init only reaps the
  children it knows about, so such a zombie keeps its record in the process
  server (though no kernel resources).
* Stopping a server that the rest of the system depends on (for instance
  `kill -STOP 3`, the console) leaves nothing runnable; the kernel reports
  the deadlock and powers off. A system server that *dies* is restarted (see
  [Boot](docs/ARCHITECTURE.md#boot)), but what it knew is lost: a
  restarted console server has lost the line being typed, and a foreground job
  loses Ctrl-C until the shell prompts again.
* Syscalls validate every user pointer against the page tables, but
  "privileged" (boot modules and the servers in `/etc/servers`, or not) is
  the only permission model; there are no capabilities yet, and no file
  permissions either: anyone may write `/etc/servers` or `/sbin`, though that
  only matters at the next boot, which starts from a fresh ramdisk.
