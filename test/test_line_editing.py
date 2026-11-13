# SPDX-License-Identifier: GPL-2.0
"""Line editing tests.

Checks the echo and line editing of the console server's line discipline, as seen by the shell.
"""
from haisuli import PROMPT

BS = "\b \b"  # How the console erases one character on screen.


def test_typed_characters_are_echoed(sh):
    echo, out = sh.run_raw("echo hi\r")
    assert echo == "echo hi"
    assert out == "hi\n"


def test_backspace_erases_a_character(sh):
    echo, out = sh.run_raw("echo abcX\x7fd\r")  # DEL, as sent by most terminals.
    assert echo == "echo abcX" + BS + "d"
    assert out == "abcd\n"


def test_ctrl_h_erases_a_character(sh):
    echo, out = sh.run_raw("echo abcX\x08d\r")
    assert echo == "echo abcX" + BS + "d"
    assert out == "abcd\n"


def test_backspace_on_an_empty_line_does_nothing(sh):
    echo, out = sh.run_raw("\x7f\x7f\x08echo ok\r")
    assert echo == "echo ok"
    assert out == "ok\n"


def test_backspace_can_erase_the_whole_line(sh):
    echo, out = sh.run_raw("help" + "\x7f" * 4 + "echo retyped\r")
    assert echo == "help" + 4 * BS + "echo retyped"
    assert out == "retyped\n"


def test_backspace_erases_a_whole_utf8_character(sh):
    echo, out = sh.run_raw("echo caf\u00e9\x7fe \u2713\x7f!\r")
    assert out == "cafe !\n"
    assert echo == "echo café" + BS + "e ✓" + BS + "!"


def test_ctrl_u_kills_the_line(sh):
    echo, out = sh.run_raw("garbage\x15echo killed\r")
    assert echo == "garbage" + 7 * BS + "echo killed"
    assert out == "killed\n"


def test_ctrl_c_abandons_the_line(sh):
    sh.send("echo never\x03")
    assert sh.expect("^C\n") == "echo never^C\n"
    # Ctrl-C sends SIGINT to the terminal's foreground process group, which is
    # the shell's between jobs. The shell ignores the signal, but its read is
    # interrupted, so it abandons the line and prompts again.
    assert sh.expect(PROMPT) == PROMPT
    echo, out = sh.run_raw("echo after\r")
    assert (echo, out) == ("echo after", "after\n")
    assert "\nnever\n" not in sh.transcript


def test_cursor_keys_are_ignored(sh):
    # Up, Left (CSI), a CSI with parameters (Ctrl-Right), and F1 (SS3).
    echo, out = sh.run_raw("echo ab\x1b[A\x1b[D\x1b[1;5Cc\x1bOP\r")
    assert echo == "echo abc"
    assert out == "abc\n"


def test_other_control_characters_are_ignored(sh):
    echo, out = sh.run_raw("echo a\x01\x02\x05\x07b\r")
    assert echo == "echo ab"
    assert out == "ab\n"


def test_cr_lf_counts_as_one_newline(sh):
    assert sh.run_raw("echo one\r\n")[1] == "one\n"
    # A second newline would have produced an extra (empty) command and prompt.
    assert sh.run_raw("echo two\r")[1] == "two\n"


def test_lf_alone_ends_a_line(sh):
    assert sh.run_raw("echo lf\n")[1] == "lf\n"


def test_overlong_lines_are_truncated(sh):
    # A line holds 255 characters; the console rings the bell for the rest.
    echo, out = sh.run_raw("echo " + "x" * 300 + "\r")
    assert out == "x" * 250 + "\n"
    assert echo == "echo " + "x" * 250 + "\a" * 50


def test_typeahead_is_processed_in_order(sh):
    # Three lines typed before the shell has handled the first one.
    sh.send("cat /1\rcat /2\rcat /3\r")
    text = "".join(sh.expect("haisuli# ") for _ in range(3))
    # Keystroke echo may be interleaved with the shell's output, even within a
    # line (as on any terminal), but each command's output is written in one
    # piece and does not occur in the echo of the typed commands.
    errors = ["cat: /1: no such file\n", "cat: /2: no such file\n", "cat: /3: no such file\n"]
    positions = [text.find(e) for e in errors]
    assert -1 not in positions and positions == sorted(positions), text
