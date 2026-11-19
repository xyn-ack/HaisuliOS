# SPDX-License-Identifier: MIT
#
# HaisuliOS build: the kernel, the user programs, the boot ramdisk, and
# targets to run and test the system in QEMU.
#
# ARCH=x86_64 (the default: a PC) or ARCH=aarch64 (QEMU's virt machine).

ARCH    ?= x86_64
TARGET  := $(ARCH)-unknown-none
REL     := target/$(TARGET)/release
ELF     := $(REL)/haisulios

# Programs in the ramdisk, by directory.
SBIN     := console
BIN      := echo true false nproc probe_test smp_test

# CPUs of the machine that `make run` boots (up to 16).
CPUS ?= 1

PYTHON ?= python3

# The toolchain for the arm64 build, as cargo's `+toolchain`. By default the one that
# rust-toolchain.toml picks; if that lacks the aarch64-unknown-none target (some stable releases
# do not ship it), 1.98.0, which does.
ifeq ($(origin AARCH64_TOOLCHAIN),undefined)
  ifeq ($(wildcard $(shell rustc --print sysroot 2>/dev/null)/lib/rustlib/aarch64-unknown-none),)
    AARCH64_TOOLCHAIN := +1.98.0
  endif
endif

ifeq ($(ARCH),x86_64)
  BUILD   := build
  # QEMU's -kernel (Multiboot v1) only accepts 32-bit ELF files.
  IMG     := $(BUILD)/haisulios.elf32
  CARGO   := cargo
  OBJCOPY ?= objcopy
  QEMU    ?= qemu-system-x86_64
  # The kernel powers off through isa-debug-exit (QEMU's exit status is then 1).
  MACHINE := -device isa-debug-exit,iobase=0xf4,iosize=0x04
else ifeq ($(ARCH),aarch64)
  BUILD   := build/aarch64
  # A Linux arm64 Image: the flat binary, whose header QEMU's -kernel knows.
  IMG     := $(BUILD)/haisulios.img
  CARGO   := cargo $(AARCH64_TOOLCHAIN)
  # GNU objcopy does not read arm64 ELF files: the toolchain's llvm-objcopy does (rustup's
  # llvm-tools component).
  OBJCOPY ?= $(firstword $(wildcard $(shell rustc $(AARCH64_TOOLCHAIN) --print sysroot 2>/dev/null)/lib/rustlib/*/bin/llvm-objcopy) llvm-objcopy)
  QEMU    ?= qemu-system-aarch64
  # The kernel powers off through PSCI SYSTEM_OFF (QEMU's exit status is then 0).
  MACHINE := -M virt,gic-version=3 -cpu cortex-a72
else
  $(error ARCH must be x86_64 or aarch64, not $(ARCH))
endif

# Boot modules, in order: init (pid 1) and the process server, which implements
# POSIX processes and exec: what it takes to load programs from the file system.
# Everything else is in the ramdisk, a ustar archive that the kernel unpacks
# into its RAM file system: rootfs/ (/etc/motd, /etc/servers), the system
# servers in /sbin (started by the process server, see /etc/servers) and the
# programs in /bin.
PROGRAMS := $(BUILD)/init $(BUILD)/proc
RAMDISK  := $(BUILD)/initrd.tar
ifeq ($(ARCH),x86_64)
  # Multiboot passes any number of modules.
  MODULES := $(BUILD)/init,$(BUILD)/proc,$(RAMDISK)
else
  # The Linux boot protocol passes one initrd: a ustar archive of the modules, in order.
  BUNDLE  := $(BUILD)/boot.tar
  MODULES := $(BUNDLE)
endif

# The serial console on the terminal, multiplexed with the QEMU monitor: Ctrl-C and other
# keys go to HaisuliOS, Ctrl-A X quits QEMU, Ctrl-A C toggles the monitor.
SERIAL := -chardev stdio,id=con,mux=on,signal=off -serial chardev:con -mon chardev=con

QEMU_FLAGS := $(MACHINE) -kernel $(IMG) -initrd "$(MODULES)" -m 128M -smp $(CPUS) $(SERIAL) \
              -display none -nic none -no-reboot

# The user crates, which build for every architecture.
USER_CRATES := haisuli-abi haisuli-rt init proc console coreutils probe_test smp_test

.PHONY: all build run debug check portcheck rustfmt rustfmtcheck clean

all: $(IMG) $(PROGRAMS) $(RAMDISK) $(BUNDLE)

# One cargo invocation builds the kernel and every user program.
build:
	$(CARGO) build --release --workspace --target $(TARGET)

ifeq ($(ARCH),x86_64)
# Re-wrap the 64-bit kernel as a 32-bit ELF for Multiboot. The kernel enters long mode itself.
$(IMG): build
	mkdir -p $(BUILD)
	$(OBJCOPY) -I elf64-x86-64 -O elf32-i386 $(ELF) $(IMG)
else
$(IMG): build
	mkdir -p $(BUILD)
	$(OBJCOPY) -O binary $(ELF) $(IMG)

# The modules in boot order (not sorted).
$(BUNDLE): $(PROGRAMS) $(RAMDISK)
	tar --format=ustar --owner=0 --group=0 -C $(BUILD) -cf $@ init proc initrd.tar
endif

# User programs are loaded by the kernel's own ELF loader: copied as-is.
$(PROGRAMS): build
	mkdir -p $(BUILD)
	cp $(REL)/$(@F) $@

# The file system: rootfs/ plus the programs. Sorted, so that directories list
# in the same order everywhere.
$(RAMDISK): build $(shell find rootfs)
	rm -rf $(BUILD)/rootfs
	mkdir -p $(BUILD)/rootfs/sbin $(BUILD)/rootfs/bin
	cp -R rootfs/. $(BUILD)/rootfs/
	cp $(addprefix $(REL)/,$(SBIN)) $(BUILD)/rootfs/sbin/
	cp $(addprefix $(REL)/,$(BIN)) $(BUILD)/rootfs/bin/
	tar --format=ustar --sort=name --owner=0 --group=0 -C $(BUILD)/rootfs -cf $@ .

# Interactive console on stdio. The kernel powers QEMU off when init exits.
run: all
	$(QEMU) $(QEMU_FLAGS)

debug: all
	$(QEMU) $(QEMU_FLAGS) -d int,cpu_reset -s -S

# Boots a fresh QEMU per test and drives the shell over the serial console.
# Pass runner options with TEST_ARGS, e.g. make check TEST_ARGS="-v echo".
# Explicit tests (a test file with EXPLICIT = "name", e.g. for a feature still
# being written) only run when named exactly, e.g.
# make check TEST_EXPLICIT="process-server" or TEST_EXPLICIT="smp"; then only they run.
# TEST_ARGS="--cpus N" boots every machine with N CPUs, unless its test asks otherwise.
check: all
	$(PYTHON) test/run_tests.py --arch $(ARCH) --qemu $(QEMU) --kernel $(IMG) \
		--modules "$(MODULES)" $(foreach name,$(TEST_EXPLICIT),--explicit "$(name)") \
		$(TEST_ARGS)

# What keeps the system portable: every user crate builds for arm64 without warnings, user space
# has no assembly outside its runtime's arch/ directory, and the kernel's generic files use the
# hardware layer through its contracts only (docs/ARCHITECTURE.md#source-layout).
portcheck:
	RUSTFLAGS="-D warnings" cargo $(AARCH64_TOOLCHAIN) build --release \
		--target aarch64-unknown-none $(addprefix -p ,$(USER_CRATES))
	@! grep -rnE 'asm!|global_asm|naked_asm|core::arch::' --include='*.rs' user \
		| grep -v '^user/rt/src/arch/' \
		|| { echo "assembly outside user/rt/src/arch/"; false; }
	@! grep -nE 'arch::(x86_64|aarch64|apic|idt|gdt|smp|entry|cpu|x86|boot|gic|psci|timer)\b|platform::(pc|qemu_virt|multiboot|acpi|pic|pit|power|uart|pl011|fdt)\b' \
		$$(find kernel -maxdepth 1 -name '*.rs') \
		|| { echo "a generic kernel file names the hardware layer's insides"; false; }
	@echo "portcheck: ok"

# Formatting, as in the Linux kernel (Documentation/rust/coding-guidelines.rst).
rustfmt:
	cargo fmt --all

rustfmtcheck:
	cargo fmt --all -- --check

clean:
	cargo clean
	rm -rf build
