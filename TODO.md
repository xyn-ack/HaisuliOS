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

1. Higher-half kernel, kernel heap, copy-on-write `fork`.
2. A futex (for `join` and a libc's locks), thread-local storage
   (`CLONE_SETTLS`), `munmap` (with TLB shootdown between CPUs), and a signal
   mask per thread.
3. More programs in `/bin`, moving commands out of the shell; a `PATH`.
4. Capability-based IPC endpoints; shared memory / page grants.
5. More user-space servers: a VFS, with the RAM file system moving out of the
   kernel into a file system server; then port musl libc.
6. Move the fd table out of the kernel (libc/VFS), so that `read`/`write`
   reach servers directly rather than via the kernel.
7. SMP that scales: per-CPU run queues instead of one lock, the I/O APIC (device
   IRQs to any CPU), timers for user space (`sleep`).

## A second CPU architecture (ARM)

HaisuliOS runs on a second CPU architecture, arm64 (aarch64), on QEMU's `virt` machine
(`make ARCH=aarch64`). It got there in small steps that each kept `make check` (and
`TEST_EXPLICIT="smp"`, `TEST_ARGS="--cpus 4"`) passing. The design: `kernel/arch/<cpu>/` for the CPU architecture and
`kernel/platform/<machine>/` for the machine around it, selected at compile time, each used by the
rest of the kernel only through what its `mod.rs` re-exports (see
[ARCHITECTURE.md](docs/ARCHITECTURE.md#source-layout)). The platform may use the architecture;
the architecture does not depend on the platform, except that the boot entry speaks the boot
loader's protocol.

Done:

1. **The split** into `arch/x86_64` and `platform/pc`, moving only.
2. **The user registers** behind the `UserRegs` trait: no generic file names a register, and the
   upcall (signal) frame's layout is the architecture's.
3. **The page tables** behind the `PageTables` trait (geometry, entry format, putting the
   kernel's half into a new address space, switching), with a generic walker in `vmm`, the
   portable `Prot`, the `AddressSpace` type and the architecture's `ELF_MACHINE`.
4. **The machine and the CPU's own devices behind contracts.** The architecture's `Processor`
   (`init_cpu(boot_cpu)`, `start_cpus`, `prepare_thread`: one call that prepares the CPU for a
   thread, on x86_64 the TSS and the `syscall` stack) and `CpuInterrupts` (the tick, the
   reschedule IPI by CPU index, end of interrupt), for `arch::Cpu`; an interrupt dispatch in
   `irq` that deals in sources (`arch::Interrupt`: tick, reschedule, line, spurious) rather than
   vectors. The platform's `Machine` (early console, start-up delays, `cpus()`, power-off),
   `BootInfo` (memory, the boot loader's own data and the boot modules, platform-neutral) and
   `InterruptController` (lines, mask, unmask, acknowledge), for `platform::Board`. No generic
   file names `arch::{apic, idt, gdt, smp, entry}`, `arch::x86_64` or `platform::pc`.
5. **The ABI and user space.** Linux numbers per architecture in `abi::nr` (declared side by side;
   `openat` and `dup3` everywhere, `open` and `dup2` on x86_64 only), with the native calls
   shared; `UC_SIGMASK` and `O_DIRECTORY` per architecture. Devices described by the machine and
   granted to drivers (`device_info`, `device_grant`) instead of `ioperm`, with I/O ports as the
   PC's extension (the opaque `arch::IoPorts`, behind the `IoPermissions` trait) and IRQ lines
   from the description. `user/rt/src/arch/{x86_64,aarch64}.rs` with all of user space's
   assembly, and `rt::device` for register access from a grant, so the console server has no
   unsafe code and chooses its UART driver (16550 or PL011). `make portcheck` keeps it so.

6. **The build, the tests, and the arm64 bring-up.** `make ARCH=x86_64|aarch64` (`make check
   ARCH=aarch64` passes the same tests, and `TEST_EXPLICIT="smp"`). `arch/aarch64`: a Linux
   Image entered at EL1, the kernel in `TTBR1_EL1` (a linear map of physical memory) and user
   space in `TTBR0_EL1`, exception vectors with the FP/SIMD state in the trap frame, `UserRegs`,
   the Linux signal frame, the context switch, `TPIDR_EL1`, the GICv3 CPU interface and the
   generic timer, PSCI `CPU_ON` and `SYSTEM_OFF`. `platform/qemu_virt`: the device tree, the
   PL011, the GICv3 distributor and redistributors, the boot modules in one initrd. The kernel
   side of memory-mapped device grants, which the console server's PL011 driver uses.

Left for later, on arm64 (none of it matters on QEMU's `virt` as the tests run it):

- Entry at EL2 (QEMU's `virtualization=on`, most real boards): the kernel expects EL1.
- Caches while the MMU is off: the boot code writes the boot tables, and the other CPUs read
  them, without cache maintenance, which QEMU does not need but real hardware does.
- Up to 1 GiB of RAM (the boot tables map 2 GiB of physical memory); one GIC redistributor
  region; the device tree's `/memreserve/` and memory nodes, but no `reserved-memory` node.
- No address space ids: every switch of address space flushes the CPU's TLB.
- QEMU's exit status on power-off is always 0 (PSCI `SYSTEM_OFF` has no code): a kernel panic
  shows in the log only.

## Smaller items

Known limitations (see the README) that are worth fixing on the way:

- An init that is not the shell, and that reaps every orphan, including the
  zombies it did not start.
- `SIGCHLD`.
- Restarting a system server that dies (like MINIX's reincarnation server).
- `execve`: close-on-exec, `#!` scripts.
- The file system: `unlink`, `mkdir`, `lseek`, file permissions.
- The shell: double quotes, `;` and pipes.
