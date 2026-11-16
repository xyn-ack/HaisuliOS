# SPDX-License-Identifier: GPL-2.0
#
# HaisuliOS build: the kernel, the user programs, the boot ramdisk, and
# targets to run and test the system in QEMU.

TARGET  := x86_64-unknown-none
REL     := target/$(TARGET)/release
ELF     := $(REL)/haisulios
IMG     := build/haisulios.elf32
OBJCOPY ?= objcopy
QEMU    ?= qemu-system-x86_64

# Boot modules, in order: init (pid 1) and the process server, which implements
# POSIX processes and exec: what it takes to load programs from the file system.
# Everything else is in the ramdisk, a ustar archive that the kernel unpacks
# into its RAM file system: rootfs/ (/etc/motd, /etc/servers), the system
# servers in /sbin (started by the process server, see /etc/servers) and the
# programs in /bin.
PROGRAMS := build/init build/proc
RAMDISK  := build/initrd.tar
MODULES  := build/init,build/proc,$(RAMDISK)
# Programs in the ramdisk, by directory.
SBIN     := console
BIN      := echo true false nproc probe_test smp_test

# COM1 on the terminal, multiplexed with the QEMU monitor: Ctrl-C and other
# keys go to HaisuliOS, Ctrl-A X quits QEMU, Ctrl-A C toggles the monitor.
SERIAL := -chardev stdio,id=con,mux=on,signal=off -serial chardev:con -mon chardev=con

# CPUs of the machine that `make run` boots (up to 16).
CPUS ?= 1

QEMU_FLAGS := -kernel $(IMG) -initrd "$(MODULES)" -m 128M -smp $(CPUS) $(SERIAL) \
              -display none -no-reboot

PYTHON ?= python3

.PHONY: all build run debug check rustfmt rustfmtcheck clean

all: $(IMG) $(PROGRAMS) $(RAMDISK)

# One cargo invocation builds the kernel and every user program.
build:
	cargo build --release --workspace --target $(TARGET)

# QEMU's -kernel (Multiboot v1) only accepts 32-bit ELF files, so re-wrap the
# 64-bit kernel. The kernel enters long mode itself.
$(IMG): build
	mkdir -p build
	$(OBJCOPY) -I elf64-x86-64 -O elf32-i386 $(ELF) $(IMG)

# User programs are loaded by the kernel's own ELF loader: copied as-is.
build/%: build
	mkdir -p build
	cp $(REL)/$* $@

# The file system: rootfs/ plus the programs. Sorted, so that directories list
# in the same order everywhere.
$(RAMDISK): build $(shell find rootfs)
	rm -rf build/rootfs
	mkdir -p build/rootfs/sbin build/rootfs/bin
	cp -R rootfs/. build/rootfs/
	cp $(addprefix $(REL)/,$(SBIN)) build/rootfs/sbin/
	cp $(addprefix $(REL)/,$(BIN)) build/rootfs/bin/
	tar --format=ustar --sort=name --owner=0 --group=0 -C build/rootfs -cf $@ .

# Interactive console on stdio. The kernel powers QEMU off via isa-debug-exit
# when init exits (QEMU's exit status is then 1; that's normal).
run: all
	$(QEMU) $(QEMU_FLAGS) -device isa-debug-exit,iobase=0xf4,iosize=0x04

debug: all
	$(QEMU) $(QEMU_FLAGS) -d int,cpu_reset -s -S

# Boots a fresh QEMU per test and drives the shell over the serial console.
# Pass runner options with TEST_ARGS, e.g. make check TEST_ARGS="-v echo".
# Explicit tests (a test file with EXPLICIT = "name", e.g. for a feature still
# being written) only run when named exactly, e.g.
# make check TEST_EXPLICIT="process-server" or TEST_EXPLICIT="smp"; then only they run.
# TEST_ARGS="--cpus N" boots every machine with N CPUs, unless its test asks otherwise.
check: all
	$(PYTHON) test/run_tests.py --qemu $(QEMU) --kernel $(IMG) --modules "$(MODULES)" \
		$(foreach name,$(TEST_EXPLICIT),--explicit "$(name)") $(TEST_ARGS)

# Formatting, as in the Linux kernel (Documentation/rust/coding-guidelines.rst).
rustfmt:
	cargo fmt --all

rustfmtcheck:
	cargo fmt --all -- --check

clean:
	cargo clean
	rm -rf build
