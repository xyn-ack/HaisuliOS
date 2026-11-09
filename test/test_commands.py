# SPDX-License-Identifier: GPL-2.0
"""Shell command tests.

Checks the shell's built-in commands (help, echo, ls, cat) and its error handling.
"""
from haisuli import read_rootfs

HELP = """\
commands:
  help            this list
  echo [text...]  print text
  ls [dir]        list files on the boot ramdisk
  cat file...     print files
  exit            leave the shell and power off
"""

# ---- help -----------------------------------------------------------------


def test_help_lists_the_commands(sh):
    assert sh.run("help") == HELP


def test_help_ignores_arguments(sh):
    assert sh.run("help me please") == HELP


# ---- echo -----------------------------------------------------------------


def test_echo_prints_its_arguments(sh):
    assert sh.run("echo hello world") == "hello world\n"


def test_echo_without_arguments_prints_an_empty_line(sh):
    assert sh.run("echo") == "\n"


def test_echo_collapses_whitespace(sh):
    assert sh.run("   echo   lots    of  space   ") == "lots of space\n"


def test_echo_treats_tabs_as_separators(sh):
    assert sh.run("echo\ta\t\tb") == "a b\n"


def test_echo_passes_utf8_through(sh):
    assert sh.run("echo héllo wörld ✓ 日本") == "héllo wörld ✓ 日本\n"


# ---- ls -------------------------------------------------------------------


def test_ls_lists_the_root_directory(sh):
    assert sh.run("ls") == "etc/\n"
    assert sh.run("ls /") == "etc/\n"


def test_ls_accepts_different_path_spellings(sh):
    for path in ["/etc", "etc", "etc/", "/etc/", "./etc", "//etc"]:
        assert sh.run(f"ls {path}") == "motd\n", path


def test_ls_reports_a_missing_directory(sh):
    assert sh.run("ls /nope") == "ls: /nope: no such directory\n"


def test_ls_of_a_file_is_an_error(sh):
    assert sh.run("ls /etc/motd") == "ls: /etc/motd: no such directory\n"


# ---- cat ------------------------------------------------------------------


def test_cat_prints_a_file(sh):
    assert sh.run("cat /etc/motd") == read_rootfs("etc/motd")


def test_cat_accepts_relative_paths(sh):
    assert sh.run("cat etc/motd") == read_rootfs("etc/motd")
    assert sh.run("cat ./etc/motd") == read_rootfs("etc/motd")


def test_cat_concatenates_several_files(sh):
    assert sh.run("cat /etc/motd /etc/motd") == 2 * read_rootfs("etc/motd")


def test_cat_reports_a_missing_file(sh):
    assert sh.run("cat /etc/nope") == "cat: /etc/nope: no such file\n"


def test_cat_refuses_directories(sh):
    assert sh.run("cat /etc") == "cat: /etc: is a directory\n"
    assert sh.run("cat /") == "cat: /: is a directory\n"


def test_cat_continues_after_an_error(sh):
    out = sh.run("cat /nope /etc/motd /etc")
    motd = read_rootfs("etc/motd")
    assert out == "cat: /nope: no such file\n" + motd + "cat: /etc: is a directory\n"


def test_cat_without_arguments_prints_nothing(sh):
    assert sh.run("cat") == ""


# ---- errors and odd input ---------------------------------------------------


def test_unknown_command(sh):
    assert sh.run("frobnicate --now") == "frobnicate: command not found (try 'help')\n"


def test_commands_are_case_sensitive(sh):
    assert sh.run("HELP") == "HELP: command not found (try 'help')\n"


def test_empty_and_blank_lines_do_nothing(sh):
    assert sh.run("") == ""
    assert sh.run("    ") == ""
    assert sh.run("\t") == ""


def test_invalid_utf8_input_is_rejected(sh):
    _, out = sh.run_raw(b"echo \xff\xfe\r")
    assert out == "init: input is not valid UTF-8\n"
    assert sh.run("echo still fine") == "still fine\n"


def test_many_commands_in_a_row(sh):
    # Every command is several IPC round trips (read, write) through the
    # console server; make sure nothing leaks or stalls over time.
    for i in range(200):
        assert sh.run(f"echo {i}") == f"{i}\n"
