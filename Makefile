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

# Boot modules, in order: the first program is init (pid 1). The ramdisk is
# a ustar archive of rootfs/ (it provides /etc/motd), which the kernel unpacks
# into its RAM file system.
PROGRAMS := build/init build/console
RAMDISK  := build/initrd.tar
MODULES  := build/init,build/console,$(RAMDISK)

# Programs that only tests boot, as extra boot modules (see EXTRA_MODULES in
# test/run_tests.py).
TEST_PROGRAMS := build/proctest

# COM1 on the terminal, multiplexed with the QEMU monitor: Ctrl-C and other
# keys go to HaisuliOS, Ctrl-A X quits QEMU, Ctrl-A C toggles the monitor.
SERIAL := -chardev stdio,id=con,mux=on,signal=off -serial chardev:con -mon chardev=con

QEMU_FLAGS := -kernel $(IMG) -initrd "$(MODULES)" -m 128M $(SERIAL) \
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

$(RAMDISK): $(shell find rootfs)
	mkdir -p build
	tar --format=ustar -C rootfs -cf $@ .

# Interactive console on stdio. The kernel powers QEMU off via isa-debug-exit
# when init exits (QEMU's exit status is then 1; that's normal).
run: all
	$(QEMU) $(QEMU_FLAGS) -device isa-debug-exit,iobase=0xf4,iosize=0x04

debug: all
	$(QEMU) $(QEMU_FLAGS) -d int,cpu_reset -s -S

# Boots a fresh QEMU per test and drives the shell over the serial console.
# Pass runner options with TEST_ARGS, e.g. make check TEST_ARGS="-v echo".
# Tests of an explicit target (work in progress, expected to fail) run only
# when TEST_EXPLICIT names the target exactly, and then only they run, e.g.
# make check TEST_EXPLICIT="process-server".
check: all $(TEST_PROGRAMS)
	$(PYTHON) test/run_tests.py --qemu $(QEMU) --kernel $(IMG) --modules "$(MODULES)" \
		$(foreach target,$(TEST_EXPLICIT),--explicit "$(target)") $(TEST_ARGS)

# Formatting, as in the Linux kernel (Documentation/rust/coding-guidelines.rst).
rustfmt:
	cargo fmt --all

rustfmtcheck:
	cargo fmt --all -- --check

clean:
	cargo clean
	rm -rf build
