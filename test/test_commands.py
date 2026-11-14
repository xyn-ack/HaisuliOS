# SPDX-License-Identifier: GPL-2.0
"""Shell command tests.

Checks the shell's built-in commands (help, echo, ls, cat) and its error handling.
"""
from haisuli import PROMPT, read_rootfs

HELP = """\
commands:
  help              this list
  echo [text...]    print text
  ls [dir]          list a directory
  cat [file...]     print files (or the input)
  ps                list processes
  pause             wait for a signal (as a job: pause &)
  kill [-sig] pid   signal a process (-pid: a process group)
  wait [pid...]     wait for background jobs
  trap [cmd sig...] run cmd on a signal ('' ignores, - resets)
  exit [status]     leave the shell and power off
  program [args...] run a program in /bin (or give its path)
end a command with & to run it in the background; $? is its status
redirect with > file, >> file, < file, 2> file and 2>&1
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
    assert sh.run("ls") == "bin/\netc/\nsbin/\n"
    assert sh.run("ls /") == "bin/\netc/\nsbin/\n"


def test_ls_accepts_different_path_spellings(sh):
    for path in ["/etc", "etc", "etc/", "/etc/", "./etc", "//etc"]:
        assert sh.run(f"ls {path}") == "motd\nservers\n", path


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


def test_cat_without_arguments_reads_its_input(sh):
    # POSIX: without operands, cat copies its standard input; Ctrl-D on an
    # empty line ends it at once.
    sh.send("cat\r")
    sh.send("\x04")
    assert sh.expect(PROMPT) == "cat\n" + PROMPT
    # The Ctrl-D was cat's, not the shell's (which would have exited).
    assert sh.run("echo still here") == "still here\n"


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
