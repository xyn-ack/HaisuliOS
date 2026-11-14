# SPDX-License-Identifier: GPL-2.0
"""Program tests: execve runs programs from the file system.

The shell runs a command that is not built in as a program: the file it names, or the file of that
name in /bin, run in a child with `execve`. The programs in /bin are `echo`, `true` and `false`,
and `probe_test`, the test program of `test_probe.py`.
A program keeps its process's pid and file descriptors, and it never runs privileged: only the
system servers that the process server starts at boot (those in /etc/servers) do. That is how the
console server runs, from /sbin/console.
"""
import re

from haisuli import PROMPT

# The kernel and the process server (which logs every exit) share the console, so their lines can
# appear anywhere in the output.
LOG_LINE = re.compile(r"\[(?:kernel|proc|console)\] [^\n]*\n")

STATUS_NOT_FOUND = 127
STATUS_NOT_EXECUTABLE = 126


def run(sh, line):
    """Like `sh.run()`, but ignores log lines."""
    sh.send(line + "\r")
    chunk = LOG_LINE.sub("", sh.expect(PROMPT))[: -len(PROMPT)]
    return chunk.partition("\n")[2]


def status(sh):
    """The shell's `$?`."""
    out = run(sh, "echo $?")
    assert re.fullmatch(r"\d+\n", out), f"`echo $?` printed {out!r}"
    return int(out)


def exits_logged_since(sh, since):
    """The `[proc] ... exited` lines since transcript position `since`, as (pid, name, status)."""
    pattern = r"\[proc\] pid (\d+) \((\S+)\) exited \(wait status (0x[0-9a-f]+)\)"
    return [(int(pid), name, int(st, 16))
            for pid, name, st in re.findall(pattern, sh.transcript[since:])]


# ---- running programs --------------------------------------------------------------


def test_a_program_runs_from_its_path(sh):
    assert run(sh, "/bin/echo hello   world") == "hello world\n"
    assert status(sh) == 0


def test_a_program_gets_its_arguments(sh):
    assert run(sh, "/bin/echo 'two  spaces' and ünïcode") == "two  spaces and ünïcode\n"
    assert run(sh, "/bin/echo") == "\n"


def test_a_name_without_a_path_runs_the_program_in_bin(sh):
    run(sh, "false")
    assert status(sh) == 1
    run(sh, "true")
    assert status(sh) == 0


def test_a_program_keeps_its_pid_and_takes_its_name(sh):
    since = len(sh.transcript)
    out = run(sh, "false &")
    m = re.search(r"^\[\d+\] (\d+)$", out, re.M)
    assert m, out
    pid = int(m[1])
    assert run(sh, f"wait {pid}") == ""
    assert status(sh) == 1
    # The child that the shell forked (its pid) became `false`.
    assert (pid, "false", 1 << 8) in exits_logged_since(sh, since)


def test_a_program_inherits_the_redirections(sh):
    assert run(sh, "/bin/echo into a file > /out") == ""
    assert run(sh, "/bin/echo and more >> /out") == ""
    assert run(sh, "cat /out") == "into a file\nand more\n"


# ---- programs that cannot run -------------------------------------------------------


def test_a_missing_program_is_not_found(sh):
    since = len(sh.transcript)
    assert run(sh, "/bin/nope") == "/bin/nope: command not found (try 'help')\n"
    assert status(sh) == STATUS_NOT_FOUND
    # The shell looks before it forks.
    assert exits_logged_since(sh, since) == []


def test_a_file_that_is_not_a_program_cannot_run(sh):
    assert run(sh, "/etc/motd") == "/etc/motd: Exec format error\n"
    assert status(sh) == STATUS_NOT_EXECUTABLE
    assert run(sh, "echo still here") == "still here\n"


def test_a_directory_cannot_run(sh):
    assert run(sh, "/bin") == "/bin: Permission denied\n"
    assert status(sh) == STATUS_NOT_EXECUTABLE


# ---- system servers -----------------------------------------------------------------


def test_the_console_server_runs_from_the_file_system(sh):
    assert "/sbin/console\n" in run(sh, "cat /etc/servers")
    assert run(sh, "ls /sbin") == "console\n"
    out = run(sh, "ps")
    rows = [line.split() for line in out.splitlines()[1:]]
    console = [row for row in rows if row[-1] == "console"]
    assert len(console) == 1, out
    assert console[0][1] == "0", f"started by the process server, a child of no one: {out!r}"


def test_a_program_never_runs_privileged(sh):
    # The console server run from the shell is an ordinary process: it may not have the UART's
    # I/O ports, so it gives up at once, and the real console server carries on.
    since = len(sh.transcript)
    run(sh, "/sbin/console")
    assert status(sh) == 1
    assert "[console] ioperm failed: errno 1\n" in sh.transcript[since:]
    assert run(sh, "echo the terminal still works") == "the terminal still works\n"
