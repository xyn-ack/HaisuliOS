# SPDX-License-Identifier: GPL-2.0
"""System server tests: the process server keeps the system servers running.

The system servers are the programs that the process server starts from /etc/servers: today the
console server. When one dies (of a signal, a fault or a non-zero exit status), the process
server starts it again, as a new process, like MINIX's reincarnation server; one that keeps
dying soon after it starts is given up. The shell survives its terminal's server dying under a
read, and gives the new one the terminal's foreground process group again.
"""
import re

from haisuli import PROMPT

# The process server's log lines can appear anywhere in the output.
LOG_LINE = re.compile(r"\[(?:kernel|proc|console)\] [^\n]*\n")

# How often in a row a system server may die soon after it starts and still be restarted
# (MAX_QUICK_FAILURES in user/proc/src/main.rs).
MAX_QUICK_FAILURES = 5


def run(sh, line):
    """Like `sh.run()`, but ignores log lines."""
    sh.send(line + "\r")
    return LOG_LINE.sub("", sh.expect(PROMPT))[: -len(PROMPT)].partition("\n")[2]


def console_pid(sh):
    """The pid of the console server, from `ps`."""
    rows = [line.split() for line in run(sh, "ps").splitlines()]
    pids = [int(row[0]) for row in rows if row and row[-1] == "console"]
    assert len(pids) == 1, f"`ps` should list one console server: {rows}"
    return pids[0]


# What the process server logs when it has started the console server again, and what the new
# console server logs when it serves: in either order (the new one runs at once).
RESTARTED = re.compile(r"\[proc\] started pid \d+ \(console\) from /sbin/console\n")
SERVING = re.compile(r"\[console\] serving [^\n]*\n")


def assert_restarted(sh):
    """The console server died and the process server started it again, which serves again."""
    sh.expect("[proc] restarting /sbin/console\n")
    seen = set()
    while len(seen) < 2:
        line = sh.expect(re.compile(f"{RESTARTED.pattern}|{SERVING.pattern}"))
        seen.add("restarted" if RESTARTED.search(line) else "serving")


def test_a_killed_console_server_is_restarted(sh):
    old = console_pid(sh)
    sh.send(f"kill -9 {old}\r")
    sh.expect(f"[proc] pid {old} (console) exited (wait status 0x9)\n")
    assert_restarted(sh)
    sh.expect(PROMPT)
    # The terminal works: input is read and echoed, output goes out.
    assert run(sh, "echo the terminal still works") == "the terminal still works\n"
    console_pid(sh)  # One console server again.


def test_ctrl_c_reaches_the_shell_after_a_restart(sh):
    # A new console server knows nothing of the old one's foreground process group: the shell
    # tells it again before it prompts, so Ctrl-C at the prompt still sends it SIGINT.
    run(sh, "trap 'echo caught INT' INT")
    sh.send(f"kill -TERM {console_pid(sh)}\r")
    assert_restarted(sh)
    sh.expect(PROMPT)
    sh.send("abandoned\x03")
    sh.expect("caught INT\n")
    sh.expect(PROMPT)
    assert run(sh, "echo after ctrl-c") == "after ctrl-c\n"


def test_the_shell_survives_its_console_dying_under_a_read(sh):
    # The job kills the console server while the shell waits for the next line: the shell's read
    # fails (the kernel fails the calls of a server that is gone), and it prompts again.
    sh.send(f"kill -9 {console_pid(sh)} &\r")
    assert_restarted(sh)
    sh.expect(PROMPT)
    assert run(sh, "echo still reading") == "still reading\n"


def test_a_server_that_keeps_dying_is_given_up(sh):
    # Every death comes soon after the start: after the last restart, the next one ends it. Then
    # nothing can use the terminal any more, and the kernel stops the machine (deadlock).
    sh.allow("deadlock")
    for _ in range(MAX_QUICK_FAILURES):
        sh.send(f"kill -9 {console_pid(sh)}\r")
        assert_restarted(sh)
        sh.expect(PROMPT)
    sh.send(f"kill -9 {console_pid(sh)}\r")
    sh.expect(f"[proc] /sbin/console failed {MAX_QUICK_FAILURES + 1} times in a row: "
              "not restarting it\n")
    sh.wait_exit()
    assert "restarting /sbin/console" not in sh.transcript.split("not restarting it")[-1]
