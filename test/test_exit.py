# SPDX-License-Identifier: MIT
"""Shell exit tests.

Checks that leaving the shell makes init exit and the process server power the machine off.
"""
from haisuli import Config


def assert_powered_off(sh, wait_status):
    # The process server reports the end of init, and powers the machine off (QEMU's exit status
    # depends on how: see haisuli.ARCHES).
    sh.expect(f"[proc] pid 1 (init) exited (wait status {wait_status})\n")
    sh.expect("[proc] init exited - shutting down\n")
    assert sh.wait_exit() == Config.arch.power_off_status


def test_exit_powers_off(sh):
    sh.send("exit\r")
    assert_powered_off(sh, "0x0")


def test_exit_after_other_commands(sh):
    assert sh.run("echo bye") == "bye\n"
    sh.send("exit\r")
    assert_powered_off(sh, "0x0")


def test_ctrl_d_on_an_empty_line_exits(sh):
    sh.send("\x04")
    assert_powered_off(sh, "0x0")


def test_ctrl_d_after_text_submits_the_line(sh):
    # Like a POSIX terminal: Ctrl-D hands over the partial line without a
    # newline (and without echoing anything); only on an empty line is it EOF.
    sh.send("echo partial\x04")
    assert sh.expect("haisuli# ") == "echo partialpartial\nhaisuli# "
    sh.send("\x04")
    assert_powered_off(sh, "0x0")
