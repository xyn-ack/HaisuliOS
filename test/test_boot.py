# SPDX-License-Identifier: GPL-2.0
"""Boot tests.

Checks that the kernel starts init and the process server, that the process server starts the
console server from the file system, and that init shows /etc/motd.
"""
import re

from haisuli import PROMPT, read_rootfs


def test_kernel_starts_init_and_console_server(sh):
    out = sh.boot_output
    assert out.startswith("HaisuliOS 0.1.0 - x86_64 microkernel\n")
    # The kernel starts tasks; the process server makes processes of them.
    # The kernel unpacks the ramdisk into its file system; init is not handed a copy.
    assert re.search(r"^\[kernel\] started task 1 \(init\): \d+ KiB image$", out, re.M)
    assert re.search(r"^\[kernel\] started task 2 \(proc\): \d+ KiB image$", out, re.M)
    # The console server is not a boot module: the process server starts it from the file
    # system, as /etc/servers says.
    assert "(console): " not in out
    assert "[proc] started pid 3 (console) from /sbin/console\n" in out
    assert re.search(r'^\[kernel\] ramdisk "initrd.tar": \d+ KiB$', out, re.M)
    assert "[console] serving COM1 (IRQ 4)\n" in out


def test_motd_is_shown_before_the_first_prompt(sh):
    assert sh.boot_output.endswith("\n" + read_rootfs("etc/motd") + PROMPT)


def test_motd_appears_exactly_once(sh):
    motd = read_rootfs("etc/motd")
    assert sh.boot_output.count(motd) == 1


def test_shell_responds_after_boot(sh):
    assert sh.run("echo ready") == "ready\n"
