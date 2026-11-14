# SPDX-License-Identifier: GPL-2.0
"""Shell redirection tests.

Checks the POSIX redirections of the shell, which the kernel backs with per-process file
descriptors and a RAM file system (the boot ramdisk, unpacked, and writable):

* `> file` writes a command's standard output to a file (created or truncated), `>> file`
  appends to it, and `< file` reads its standard input from one.
* `n>`, `n>>` and `n<` do the same for file descriptor n, and `n>&m` makes n a copy of m, so
  `2>&1` sends errors where the output goes. Redirections apply from left to right.
* A redirection may come before, between or after the words of a command, with or without a
  space before its target, and applies to that command only.
* Commands report errors on standard error.
* `cat` without operands copies its standard input.

File names without a leading `/` are taken from the root directory.
"""
import re

from haisuli import PROMPT, read_rootfs

# The kernel logs the exit of every child process (here: background jobs) on the console.
KERNEL_LINE = re.compile(r"\[kernel\] [^\n]*\n")
MOTD = read_rootfs("etc/motd")


def run(sh, line):
    """Like `sh.run()`, but ignores kernel log lines."""
    sh.send(line + "\r")
    chunk = KERNEL_LINE.sub("", sh.expect(PROMPT))[: -len(PROMPT)]
    return chunk.partition("\n")[2]


def status(sh):
    """The shell's `$?`."""
    out = run(sh, "echo $?")
    assert re.fullmatch(r"\d+\n", out), f"`echo $?` printed {out!r}"
    return int(out)


# ---- output: > and >> ------------------------------------------------------------


def test_output_redirection_writes_a_file_instead_of_the_terminal(sh):
    assert run(sh, "echo hello > greeting") == ""
    assert status(sh) == 0
    assert run(sh, "cat greeting") == "hello\n"


def test_output_redirection_truncates_the_file(sh):
    run(sh, "echo a much longer first line > f")
    run(sh, "echo second > f")
    assert run(sh, "cat f") == "second\n"


def test_append_redirection_adds_to_the_end(sh):
    run(sh, "echo one > f")
    run(sh, "echo two >> f")
    run(sh, "echo three >> f")
    assert run(sh, "cat f") == "one\ntwo\nthree\n"


def test_append_redirection_creates_a_missing_file(sh):
    run(sh, "echo first >> new")
    assert run(sh, "cat new") == "first\n"


def test_redirections_may_come_anywhere_in_the_command(sh):
    assert run(sh, ">before echo one") == ""
    assert run(sh, "echo two >attached") == ""
    assert run(sh, "echo three > between four") == ""
    assert run(sh, "cat before attached between") == "one\ntwo\nthree four\n"


def test_a_redirection_applies_to_its_command_only(sh):
    run(sh, "echo to the file > f")
    assert run(sh, "echo to the terminal") == "to the terminal\n"
    assert run(sh, "cat f") == "to the file\n"


def test_a_redirection_alone_creates_an_empty_file(sh):
    assert run(sh, "> empty") == ""
    assert run(sh, "cat empty") == ""
    assert run(sh, "ls") == "bin/\netc/\nsbin/\nempty\n"


def test_created_files_are_listed_in_their_directory(sh):
    run(sh, "echo x > /etc/notes")
    assert run(sh, "ls /etc") == "motd\nservers\nnotes\n"


def test_a_large_file_survives_the_round_trip(sh):
    # The message of the day spans several writes of the shell's output buffer.
    run(sh, "cat /etc/motd > copy")
    assert run(sh, "cat copy") == MOTD


# ---- input: < ---------------------------------------------------------------------


def test_input_redirection_feeds_a_file_to_a_command(sh):
    run(sh, "echo line one > f")
    run(sh, "echo line two >> f")
    assert run(sh, "cat < f") == "line one\nline two\n"


def test_input_and_output_redirections_together(sh):
    run(sh, "cat < /etc/motd > copy")
    assert run(sh, "cat copy") == MOTD


def test_cat_without_operands_copies_the_terminal_until_ctrl_d(sh):
    sh.send("cat\r")
    sh.send("typed line\r")
    assert sh.expect("typed line\ntyped line\n").endswith("typed line\ntyped line\n")
    sh.send("\x04")  # End of input.
    sh.expect(PROMPT)
    assert run(sh, "echo back at the prompt") == "back at the prompt\n"


# ---- standard error and 2>&1 --------------------------------------------------------


def test_errors_go_to_standard_error(sh):
    assert run(sh, "cat /nope > out") == "cat: /nope: no such file\n"
    assert run(sh, "cat out") == ""
    assert run(sh, "cat /nope 2> err") == ""
    assert run(sh, "cat err") == "cat: /nope: no such file\n"


def test_2_to_1_sends_errors_where_the_output_goes(sh):
    assert run(sh, "cat /etc/motd /nope > both 2>&1") == ""
    # One open file, shared by both descriptors: the error follows the output.
    assert run(sh, "cat both") == MOTD + "cat: /nope: no such file\n"


def test_redirections_apply_from_left_to_right(sh):
    # `2>&1` first: errors go where the output goes *then*, the terminal.
    assert run(sh, "cat /nope 2>&1 > out") == "cat: /nope: no such file\n"
    assert run(sh, "cat out") == ""


# ---- errors -----------------------------------------------------------------------


def test_output_into_a_missing_directory_fails(sh):
    out = run(sh, "echo never > /nope/f")
    assert "No such file or directory" in out, out
    assert "never" not in out
    assert status(sh) == 1
    assert run(sh, "echo still on the terminal") == "still on the terminal\n"


def test_input_from_a_missing_file_fails_without_running_the_command(sh):
    out = run(sh, "echo ran < missing")
    assert "No such file or directory" in out, out
    assert "ran" not in out
    assert status(sh) == 1


def test_output_into_a_directory_fails(sh):
    out = run(sh, "echo never > /etc")
    assert "Is a directory" in out, out
    assert status(sh) == 1


def test_a_redirection_needs_a_target(sh):
    out = run(sh, "echo x >")
    assert out != "" and "x" not in out.split(), out
    assert status(sh) == 2  # A syntax error.


# ---- jobs -------------------------------------------------------------------------


def test_a_background_job_writes_to_its_redirection(sh):
    out = run(sh, "echo from the job > jobout &")
    m = re.search(r"^\[\d+\] (\d+)$", out, re.M)
    assert m, out
    run(sh, f"wait {m[1]}")
    assert status(sh) == 0
    assert run(sh, "cat jobout") == "from the job\n"
