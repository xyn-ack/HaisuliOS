CC      ?= gcc
PROGS   := init sh ls cat echo hello crash
# test programs, sources in test/ (pipetest is built from test/test.c)
TESTS   := pipetest pipecheck
BINS    := $(PROGS) $(TESTS)
COMMON  := -m64 -ffreestanding -fno-pic -fno-pie -fno-stack-protector -mno-red-zone \
           -mno-sse -mno-sse2 -mno-mmx -mno-avx -fno-tree-loop-distribute-patterns \
           -fcf-protection=none -fno-asynchronous-unwind-tables -O2 -Wall -Wextra
LD      := ld -m elf_x86_64 -nostdlib -z max-page-size=0x1000 -z noexecstack --no-warn-rwx-segments
KOBJ    := $(addprefix build/kernel/,boot.o entry.o main.o proc.o syscall.o pipe.o initrd.o)
UELF    := $(addprefix build/user/,$(addsuffix .elf,$(BINS)))
# user/include has the few standard headers (stdio.h, unistd.h, ...) that ported programs expect
UCFLAGS := -Iuser -Iuser/include
UHDR    := user/ulib.h $(wildcard user/include/*.h)

all: build/kernel32.elf

# QEMU's -kernel multiboot loader only accepts 32-bit ELF, so repackage the image.
build/kernel32.elf: build/kernel.elf
	objcopy -O elf32-i386 $< $@
build/kernel.elf: $(KOBJ) kernel/kernel.ld
	$(LD) -T kernel/kernel.ld -o $@ $(KOBJ)

build/kernel/%.o: kernel/%.c kernel/kernel.h
	@mkdir -p $(@D); $(CC) $(COMMON) -c $< -o $@
build/kernel/%.o: kernel/%.S
	@mkdir -p $(@D); $(CC) -m64 -c $< -o $@
build/kernel/initrd.o: kernel/initrd.S build/initrd.tar
	@mkdir -p $(@D); $(CC) -m64 -Ibuild -c $< -o $@

build/user/%.o: user/%.c $(UHDR)
	@mkdir -p $(@D); $(CC) $(COMMON) $(UCFLAGS) -c $< -o $@
build/user/%.o: test/%.c $(UHDR)
	@mkdir -p $(@D); $(CC) $(COMMON) $(UCFLAGS) -c $< -o $@
build/user/pipetest.o: test/test.c $(UHDR)
	@mkdir -p $(@D); $(CC) $(COMMON) $(UCFLAGS) -c $< -o $@
build/user/%.elf: build/user/%.o build/user/ulib.o user/user.ld
	$(LD) -T user/user.ld -o $@ $< build/user/ulib.o

build/initrd.tar: $(UELF) $(wildcard rootfs/etc/*)
	rm -rf build/rootfs && mkdir -p build/rootfs/bin
	cp -r rootfs/etc build/rootfs/etc
	for p in $(BINS); do cp build/user/$$p.elf build/rootfs/bin/$$p; done
	strip build/rootfs/bin/*
	tar --format=ustar --sort=name --owner=0 --group=0 --numeric-owner -cf $@ -C build/rootfs bin etc

run: all
	qemu-system-x86_64 -kernel build/kernel32.elf -m 128M -nographic -no-reboot
check: all
	python3 test/run.py build/kernel32.elf
clean:
	rm -rf build
.PHONY: all run check clean
