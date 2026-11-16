# Changelog

What changed in each version of HaisuliOS. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the goals and the
roadmap are in [TODO.md](TODO.md).

## Unreleased

The fourth and fifth steps toward a second CPU architecture. The machine and the CPU's own devices
are behind contracts too: generic kernel code no longer names the local APIC, the IDT, the GDT,
the SMP start-up or the entry stubs, nor any module of the PC platform; it boots, schedules and
takes interrupts through traits that the compiler checks, like `UserRegs` and `PageTables`. And
the ABI and user space are ready for arm64: system call numbers per architecture, devices granted
to drivers instead of `ioperm`, and all of user space's assembly in the runtime's `arch/`; every
user program builds for `aarch64-unknown-none` (`make portcheck`).

### Added

- `openat` (257 on x86_64, 56 on arm64), `dup3` (292, 24) and `fcntl(F_GETFD)`: the calls that
  arm64 has instead of `open` and `dup2`, which the runtime now uses everywhere. A relative path
  of `openat` starts at a directory descriptor (or at the root for `AT_FDCWD`).
- Devices for drivers: `device_info` (0x101B) describes a device the machine has (what it is,
  where its registers are, its IRQ line; `abi::device`, so far `CONSOLE`), and `device_grant`
  (0x101C) gives a privileged task its registers. The runtime's `device` module reads and writes
  granted registers, in the I/O port space or memory-mapped alike.
- The console server picks its UART driver by the device's kind: the 16550 (the PC's COM1), or
  the PL011 (arm64), and takes its IRQ line from the description.
- `make portcheck`: every user crate builds for `aarch64-unknown-none`, no assembly outside
  `user/rt/src/arch/`, and no generic kernel file names the hardware layer's insides.
- The architecture's `IoPermissions` trait (for `arch::IoPorts`): a task's I/O ports, opaque to
  generic code (the TSS's bitmap on x86_64).
- The architecture's `Processor` trait (for `arch::Cpu`): `init_cpu(boot_cpu, wait)` sets up a
  CPU (on x86_64 the GDT and TSS, the IDT, the local APIC and its timer, `syscall`),
  `start_cpus` starts the others from the hardware ids the platform lists, `prepare_thread` is
  the one call that prepares a CPU for a thread (on x86_64 the TSS's kernel stack and I/O bitmap
  and the `syscall` stack), plus `wait_for_interrupt` and `halt`.
- The architecture's `CpuInterrupts` trait: the tick, the reschedule IPI, end of interrupt. IPIs
  name CPUs by the kernel's index; the APIC keeps each CPU's APIC id itself.
- `arch::Interrupt`: the architecture's interrupt entry tells `irq::dispatch` where an interrupt
  came from (the tick, a reschedule, a platform line, or nothing) instead of passing an x86
  vector.
- The platform's traits (for `platform::Board`): `Machine` (the early console, start-up delays,
  the CPUs the firmware lists, power-off), `BootInfo` (the available memory, the boot loader's own
  data and the boot modules, without Multiboot's types) and `InterruptController` (lines, mask,
  unmask, acknowledge).

### Changed

- `abi::nr` has the architecture's Linux numbers, declared side by side for x86_64 and arm64;
  `open` and `dup2` are x86_64's only. `UC_SIGMASK` (now counted from the restorer's stack
  pointer) and `O_DIRECTORY` are per architecture too; the native calls are shared.
- `user/rt/src/arch/{x86_64,aarch64}.rs` hold all of user space's assembly: the system call,
  `_start` (which `entry!` expands to), the `clone` and thread-exit stubs, the signal restorer,
  and x86's port I/O. The console server has no unsafe code any more.
- `abi::uart` is `abi::uart16550`, without the PC's COM1 constants (the PC platform describes
  COM1 now); `abi::pl011` has the PL011's registers.
- Booting (`main.rs`) goes through the contracts only. The boot CPU sets itself up before the
  frame allocator and the 8259s rather than around them (interrupts are off throughout, so the
  order does not matter to the hardware).
- `arch::x86_64` and `platform::pc` are named only by the hardware layer: by the PC for its
  port I/O and vectors, and by x86_64's boot entry for the Multiboot handshake.
- A task's pending IRQ lines are a 64-bit set (the platform says how many lines there are; the
  compiler checks that they fit).

### Fixed

Races that made tests fail now and then when every machine had several CPUs (`make check
TEST_ARGS="--cpus 4"`), and that two new tests now provoke on purpose:

- The kernel could lose the wake-up of a thread waiting for a service to register (`svc_wait`,
  and a terminal `read` or `write` before the console server is up): it looked the service up and
  went to sleep under two holds of the scheduler's lock, and a registration on another CPU in
  between left it asleep. At boot, init then never printed anything.
- `execve` could deadlock the kernel: `task_exec` rebuilt the calling thread's context while
  holding the scheduler's lock, and waited for the thread to be off its CPU, but that CPU could
  still be about to switch away from it, which takes the same lock. It happens when a CPU is held
  up between blocking in its call and switching away (on a busy host, an emulated CPU can be
  stopped for a long time; `probe_test exec-loop` hung about once in 35 runs then). It now waits
  without the lock.
- A signal sent to a job that had only just started could meet the shell's actions instead of
  the job's: the shell ignores `SIGTERM` and catches `SIGINT` and its trapped signals, and the
  job resets them only after `fork`. `kill -15` then did nothing (and `wait` waited forever), and
  a trapped signal ran the shell's handler in the job. The shell now blocks every signal from
  before the `fork` until the job has reset them.
- The process server discarded a signal that was ignored when it was sent even if it was
  blocked, so blocking it did not help either. A blocked signal now stays pending, and the action
  it meets when it is unblocked decides, as on Linux (POSIX leaves it open).
- `echo` (the shell's and `/bin/echo`) wrote every word, and the newline, separately: a log line
  from another CPU could land in the middle of its output. It now writes its line at once.

### Added (tests)

- `probe_test blocked-ignored` (`test_probe.py`): what happens to a signal that is blocked when
  it is sent, whether or not it is ignored then and when it is unblocked.
- `probe_test files` and `probe_test devices` (`test_probe.py`): `openat` relative to a
  directory, `dup3`'s and `dup2`'s special cases, x86_64's `open` and `dup2`, the console's
  description, and that an ordinary program is granted no device.
- `test_a_job_signalled_as_soon_as_it_starts_gets_the_default_action` (`test_process.py`): types
  `kill` together with `pause &`, so that the signal arrives while the job still resets the
  shell's actions.

### Removed

- `kernel/process/` (`signal.rs`, `group.rs`), dead code left over from 0.1 that no module
  included since `kernel/process.rs` went in 0.3.1.
- `user/init/src/jobs.rs`, `signals.rs` and `ustar.rs`, which the shell no longer included.
- `user/proctest/`, a test program outside the workspace that nothing built (`probe_test`
  replaced it).
- `ioperm` (173), `IO_PORTS` and `abi::uart::{COM1, PORTS, COM1_IRQ}`: devices are granted.

## 0.3.1

In preparation for a second CPU architecture (ARM), the hardware layer is split along two axes:
`kernel/arch/` is the CPU architecture (one directory per architecture, `arch/x86_64` today,
selected at compile time) and `kernel/platform/` is the machine around it (`platform/pc`: the
boot loader's data, the 8259 PICs, the COM1 UART, the 8254 timer, the ACPI tables, power-off).
Each is used only through what its `mod.rs` re-exports: that list is the contract a new
architecture implements.

### Added

- The first compiler-checked piece of the architecture contract: the `UserRegs` trait, everything
  generic code may do with a thread's saved user registers (the system call's number and
  arguments, the return value, restarting the call, the stack pointer). No generic file names a
  register any more; the upcall frame's layout (Linux's signal frame) belongs to the
  architecture.
- The second piece, the `PageTables` trait: the page tables' shape, the format of their entries,
  and how an address space gets the kernel's half and becomes a CPU's (`arch/x86_64/paging.rs`).
  `vmm` walks them generically (one recursive walk for `task_fork`'s copy and for freeing),
  with the portable `Prot` (user, write, execute) instead of x86 page flags, and the
  `AddressSpace` type instead of a PML4 address in a `u64`. The ELF loader takes the machine type
  from the architecture (`ELF_MACHINE`).
- `phys::keep_out`: the platform records the boot loader's memory once, and `phys` refuses it
  from then on, instead of asking the platform on every check.
- [TODO.md](TODO.md) lists the steps that remain before arm64 (the interrupt controller and
  timers, booting, the ABI and user space, the build and the bring-up).

### Changed

- The GDT, IDT and TSS moved into `arch/x86_64` (the IDT now installs every gate itself); the
  8259, COM1, 8254, ACPI, Multiboot and power-off code into `platform/pc`. The local APIC's
  timer calibration and CPU start-up take the platform's `wait` function instead of using the
  PIT themselves.
- The `restart` and `eintr` cases of `probe_test` no longer guess when the parent waits: the
  parent announces the wait (a file), the child watches `ps` until the parent sleeps in it, and
  only then signals. They are exact on any number of CPUs now, and `eintr` joined the
  side-by-side SMP test. `pause-race`, which is about one CPU's order, always runs on one CPU
  (`@cpus(1)`), also under `--cpus`.

### Removed

- The dead files `kernel/process.rs` and `kernel/signal.rs`, left over from 0.1.

## 0.3.0

Multiple CPUs, threads and preemption. The kernel runs on every CPU of the machine, a task can
have any number of threads, and the scheduler preempts; the process server makes threads POSIX
(one pid, one exit, signals), and the runtime gives programs `thread::spawn` and `join`. The tests
came first (`test_smp.py`).

### Added

- SMP: the boot CPU starts the other CPUs listed in the ACPI MADT (up to 16) through a real-mode
  trampoline, and logs how many run (`[kernel] 4 CPUs online`). Every CPU has a per-CPU block
  (GS, `swapgs`), its own TSS and `syscall` stack, a local APIC timer and an idle context; CPUs
  wake each other with a reschedule IPI.
- Preemption: the APIC timer ticks every 10 ms; a thread that has run for its quantum (two ticks)
  while another waits gives way on its way back to user mode. Upcalls are entered on every way
  back to user mode, after interrupts too.
- Threads, with the Linux calls (and numbers) `clone` (56, `clone::THREAD_FLAGS` only), `exit`
  (60, of one thread), `gettid` (186), `sched_getaffinity` (204) and `getcpu` (309). A task ends
  when its last thread does, and all its threads end with it.
- `serial_write` (0x101A): the console server's output goes through the kernel's transmitter, so
  it never mixes with the kernel's log lines on several CPUs.
- The runtime: `thread::spawn`, `Thread::join`, `sys::gettid`, `getcpu`, `sched_getaffinity`,
  `nprocs`, `thread_exit`, `mmap` and `serial_write`.
- The process server handles threads: any thread may make requests; `fork` copies the calling
  thread only; `execve` ends the other threads; several threads may wait in `pause` or `wait4`;
  a caught signal goes to a waiting thread (whose wait it ends), and runs before the reply to a
  request of its process (POSIX's rule for `sigprocmask` and `kill` to oneself).
- `/bin/nproc`; `ps` shows the threads of every process (`THR`); `make run CPUS=4`.
- Tests: `test_smp.py`, explicit tests of multiple CPUs and threads (run with
  `make check TEST_EXPLICIT="smp"`), and the test program `/bin/smp_test` (`user/smp_test`).
- Test runner: the `@cpus(n)` decorator (`haisuli.cpus`) boots a test's machine with `n` CPUs;
  `--cpus N` (e.g. `make check TEST_ARGS="--cpus 4"`) sets the number for every other test; a test
  that asks for `n` CPUs holds `n` of the host's while it runs (`--cpu-budget`).

### Changed

- A task is an address space with one or more threads; thread and task ids share one range (a
  task's id is its first thread's), and are handed out round robin. `MAX_THREADS` is 64; still
  at most 16 tasks.
- `ipc_recv` fills in four words, `[sender, a, b, task]`: for a call, the calling thread (to reply
  to) and its task. `task_fork` and `task_exec` take the calling thread; `task_upcall` takes a
  thread with the new `upcall::THREAD` flag. `TaskInfo` and `ProcInfo` report thread counts.
- `Lock` is a ticket spinlock (it still panics on re-entrant use); the kernel log takes a lock per
  message. A dying task's memory and descriptors are freed once no CPU uses them any more.
- The console server sends its output with `serial_write` instead of its own port I/O (it still
  owns the UART's input).

### Fixed

- The frame allocator could give the same frame to two CPUs at once.
- The test harness left QEMU running when a machine never reached its first prompt.

## 0.2.0

POSIX process management moves out of the kernel into a user-space process
server, and programs load from the file system. The kernel now knows only
*tasks*: it provides mechanisms, and the process server builds processes,
signals and `execve` on top of them. The servers that booting does not need
come from the file system too.

### Added

- The **process server** (`user/proc`), a boot module (pid 2). It keeps the
  process table and implements, on top of the kernel's tasks: pids (separate
  from kernel task ids), parents and children, `fork`, `execve`, `exit` and
  `wait4`, zombies and wait statuses, `getpid` and `getppid`, process groups
  and sessions (`setpgid`, `getpgid`, `setsid`, `getsid`), `kill` and the
  terminal's `kill_pgrp`, signals (dispositions, masks, pending signals,
  default actions, stop and continue) and the listing behind `ps`. It logs the
  end of every process (`[proc] pid N (name) exited (wait status 0x...)`) and
  powers the machine off when init exits. Its protocol is `abi::proc`; the
  user runtime keeps the same API and turns the calls into requests to it.
- Kernel **task control** for the *task manager*, the task registered as
  service 2 (the process server): `task_fork`, `task_kill`, `task_stop`,
  `task_cont`, `task_upcall`, `task_exec` and `task_spawn`, with reports of
  tasks that end by themselves (`KMSG_TASK_EXIT`, `KMSG_TASK_FAULT`). Also
  `task_info`, `task_self`, `task_exit`, `svc_wait` and `power_off`.
- **Upcalls** (`task_upcall`): the kernel mechanism under signal handlers. The
  kernel enters a handler with a Linux `rt_sigframe` on the way back to user
  mode, ends an interruptible wait with `EINTR`, and can make the interrupted
  call again (`SA_RESTART`).
- **`execve`**: static ELF programs run from the file system, with `argv`
  and `envp` on the System V initial stack (`StartInfo::args()` and `env()`
  in the runtime). Argument blocks are `abi::exec`; new error numbers `E2BIG`
  and `ENOEXEC`.
- The shell runs a command that is not built in as a program: by its path,
  or by its name in `/bin` (status 127 if there is none, 126 if it cannot
  run). `help` lists this.
- Programs in `/bin`: `echo`, `true` and `false` (`user/coreutils`), and the
  test program `probe_test` (`user/probe_test`).
- `/etc/servers`: the system servers that the process server starts at boot,
  privileged. Today that is `/sbin/console`.
- Tests: `test_process_server.py` (explicit tests, run with
  `make check TEST_EXPLICIT="process-server"`), `test_exec.py` and
  `test_probe.py`, which runs `/bin/probe_test` for what the shell cannot
  reach (`SA_RESTART`, faults, `task_exit`, back-to-back signals, a race of
  `pause` in the process server, `execve` semantics, 1000 `execve`s in a row).
- Test runner: *explicit* tests (`EXPLICIT = "name"` in a test file), which
  run only with `--explicit name` / `TEST_EXPLICIT`; `Machine.allow()` for
  tests that cause a crash on purpose; `Machine.wait_exit()` waits for the
  last console output.
- `CHANGELOG.md`, `TODO.md`, `docs/ARCHITECTURE.md` and `docs/TESTING.md`, split out of
  the README, which keeps the overview.

### Changed

- The kernel's process table is a task table (`kernel/process.rs` became
  `kernel/task.rs`); of `kernel/signal.rs` only the frame of an upcall is
  left (`kernel/upcall.rs`); `kernel/program.rs` builds the address space of a
  program.
- Boot modules: init and the process server (`build/init,build/proc,build/initrd.tar`).
  The console server is no longer one: the process server starts it from
  `/sbin/console`, as pid 3, with no parent. Jobs get pids from 4 on.
- The ramdisk contains the programs of `/bin` and `/sbin` besides `rootfs/`,
  packed in sorted order.
- The kernel's boot log says `started task N` (was `started pid N`). Exits
  are logged by the process server, not the kernel; shutting down logs
  `[proc] init exited - shutting down` and `[kernel] powering off` (was
  `[kernel] init exited - shutting down`).
- A zombie no longer holds one of the kernel's 16 task slots: it is only a
  record in the process server, which keeps up to 64 processes.
- `kill -0` of a zombie succeeds, as POSIX says. The process server cannot be
  signalled (`EPERM`).
- Privileges: boot modules and the servers in `/etc/servers` are privileged;
  forked children and programs started with `execve` never are.
- Files may be up to 128 KiB (was 64 KiB).
- `ps` lists up to 64 processes (was 16).
- A user-space panic names the task: `[task N] panic: ...` (was `[pid N]`).
- The kernel's banner takes its version from `Cargo.toml`.

### Removed

- The kernel's system calls of POSIX process management, now requests to the
  process server: `rt_sigaction` (13), `rt_sigprocmask` (14), `pause` (34),
  `getpid` (39), `fork` (57), `exit` (60), `wait4` (61), `kill` (62),
  `setpgid` (109), `getppid` (110), `setsid` (112), `getpgid` (121),
  `getsid` (124), `proc_list` (0x100B) and `kill_pgrp` (0x100C). The kernel
  answers them with `ENOSYS`.
- The unused source files `kernel/process/group.rs` and
  `kernel/process/signal.rs`.

## 0.1.0

The first version: a microkernel with POSIX processes in the kernel.

- Kernel: Multiboot v1 boot with its own switch to 64-bit mode, a bitmap
  frame allocator, 4-level paging with a private address space per process
  and W^X, a cooperative round-robin scheduler, and an ELF loader for boot
  modules. Unsafe code is confined to `kernel/arch` and `kernel/sync.rs`.
- Processes in the kernel: `fork`, `exit`, `wait4`, `getpid`, `getppid`,
  process groups and sessions, and signals with POSIX job control (handlers
  with a Linux signal frame, masks, stop and continue, `SA_RESTART`).
- IPC: asynchronous notifications and synchronous calls whose servers copy
  data from and to the client's buffer, as far as the call allows; IRQ
  forwarding and I/O-port permissions for user-space drivers; well-known
  services.
- A RAM file system filled from the boot ramdisk, with POSIX file
  descriptors and open file descriptions.
- User space: the console server (COM1 driver, line discipline, job control
  terminal) and init, which shows `/etc/motd` and runs a job control shell
  with redirections and `trap`. Both are boot modules.
- Tests that boot QEMU and drive the shell (`make check`).
