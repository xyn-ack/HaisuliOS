# SPDX-License-Identifier: MIT
"""Process server tests: POSIX process management moves out of the kernel.

These tests were written before the process server existed (test first). They are explicit tests:
`make check` skips them, and

    make check TEST_EXPLICIT="process-server"

runs them (and only them). Now that the server works, removing `EXPLICIT` makes them ordinary
tests of `make check`.

A microkernel should provide mechanisms only: address spaces, execution contexts, scheduling, IPC.
Pids, parents and children, `fork`, `exit` statuses and `wait4`, zombies, `getpid` and `getppid`,
process groups and sessions are POSIX concepts. They move into a user-space *process server*, just
as the console server took the terminal out of the kernel (MINIX calls its version the process
manager, PM). Programs keep making the same calls. The runtime turns them into IPC calls to the
server, and the server uses kernel mechanisms to carry them out.

The shell cannot see where a call is served, so the tests check what a user can see:

* The server is a boot module named `proc`. `ps` lists it as a process of its own, a child of no
  one (PPID 0) like the other boot modules. init is still pid 1.
* Like the console server, it announces itself on the debug log with a line starting with
  `[proc] serving`.
* Exit statuses belong to the server, not the kernel. The server logs the end of every process as
  `[proc] pid N (name) exited (wait status 0x...)` with its POSIX wait status, in the format that
  the kernel used before. The kernel's log never mentions a wait status.
* A zombie is only the server's record of an exit status that the parent has not collected yet.
  It holds no kernel resources, so more zombies can exist than the kernel has process slots.
* The rest of the behaviour stays the same: `test_process.py` covers fork, wait and signal
  semantics, and reads the exit statuses from the `[proc]` lines too.
"""
import re
import time

from haisuli import PROMPT, Config, Timeout

# Run by `make check TEST_EXPLICIT="process-server"` only (see run_tests.py).
EXPLICIT = "process-server"

# The name of the process server's program, as `ps` shows it and as it tags its log lines.
SERVER = "proc"

SIGKILL = 9

# The number of tasks the kernel has room for (`MAX_TASKS` in kernel/task.rs). A zombie used to
# hold one of them until it was reaped, back when the kernel implemented processes.
KERNEL_PROCESS_SLOTS = 16


# The kernel and the process server log to the same serial line as the console server, so their
# lines can appear anywhere in the output, even in the middle of an echoed command line.
LOG_LINE = re.compile(rf"\[(?:kernel|{SERVER})\] [^\n]*\n")

# Job status notices ("[1]+ Done  exit 7") that a shell may print before a prompt, maybe after an
# empty line. Unlike the "[1] 3" of `cmd &`, the job number is not followed by a pid.
JOB_NOTICE = re.compile(r"^\n?\[\d+\][+-]? *[^\d\s][^\n]*\n", re.M)


class Job:
    """A background job: its pid, and where the console transcript stood when it started."""

    def __init__(self, pid, since):
        self.pid = pid
        self.since = since


# ---- helpers ------------------------------------------------------------------


def run(sh, line):
    """Like `sh.run()`, but ignores log lines and job status notices."""
    sh.send(line + "\r")
    try:
        chunk = LOG_LINE.sub("", sh.expect(PROMPT))[: -len(PROMPT)]
    except Timeout:
        assert "init exited" not in sh.transcript, f"`{line}` made the shell exit"
        raise
    return JOB_NOTICE.sub("", chunk.partition("\n")[2])


def status(sh):
    """The shell's `$?`."""
    out = run(sh, "echo $?")
    assert re.fullmatch(r"\d+\n", out), f"`echo $?` printed {out!r}"
    return int(out)


def start(sh, command):
    """Starts `command &` and returns the job."""
    since = len(sh.transcript)
    out = run(sh, f"{command} &")
    m = re.search(r"^\[\d+\] (\d+)$", out, re.M)
    assert m, f"`{command} &` should print the job number and pid ('[1] 3'), printed {out!r}"
    return Job(int(m[1]), since)


def wait_status(sh, pid):
    """Runs `wait pid` and returns its status."""
    out = run(sh, f"wait {pid}")
    assert out == "", f"`wait {pid}` printed {out!r}"
    return status(sh)


def ps(sh):
    """The `ps` listing as {pid: {column: value}}."""
    out = run(sh, "ps")
    lines = out.splitlines()
    for i, line in enumerate(lines):
        header = line.split()
        if {"PID", "PPID", "NAME"} <= set(header):
            break
    else:
        raise AssertionError(f"`ps` should print a header with PID, PPID and NAME: {out!r}")
    procs = {}
    for line in lines[i + 1:]:
        if line.strip():
            row = dict(zip(header, line.split()))
            assert row.get("PID", "").isdigit(), f"bad `ps` line {line!r} in {out!r}"
            procs[int(row["PID"])] = row
    return procs


def wait_for_log(sh, pattern, since, what):
    """Waits until the transcript from position `since` on matches `pattern`; returns the match.

    Log lines are not tied to any prompt, so this polls the transcript instead of consuming it.
    """
    deadline = time.monotonic() + sh.config.timeout
    while True:
        m = pattern.search(sh.transcript, since)
        if m:
            return m
        assert time.monotonic() < deadline, f"no {what} on the console"
        time.sleep(0.05)


def server_wait_status(sh, job):
    """Waits until the process server has logged the end of `job`; returns the wait status.

    Pids are reused, so only the log since the job started counts.
    """
    pattern = re.compile(
        rf"\[{SERVER}\] pid {job.pid} \(\S+\) exited \(wait status (0x[0-9a-f]+)\)$", re.M)
    m = wait_for_log(sh, pattern, job.since, f"[{SERVER}] line for the exit of pid {job.pid}")
    return int(m[1], 16)


def kernel_wait_statuses(sh):
    """Kernel log lines that mention a wait status (there should be none)."""
    return re.findall(r"\[kernel\][^\n]*wait status[^\n]*", sh.transcript)


# ---- the server ---------------------------------------------------------------


def test_the_process_server_announces_itself(sh):
    # Like "[console] serving COM1 (IRQ 4)". It may come after the first prompt.
    wait_for_log(sh, re.compile(rf"^\[{SERVER}\] serving\b", re.M), 0,
                 f"'[{SERVER}] serving' line")


def test_ps_lists_the_process_server(sh):
    procs = ps(sh)
    servers = [row for row in procs.values() if row.get("NAME") == SERVER]
    assert len(servers) == 1, f"`ps` should list one {SERVER!r} process: {procs}"
    assert servers[0]["PPID"] == "0", f"a boot module is a child of no one: {servers[0]}"
    assert procs.get(1, {}).get("NAME") == "init", f"init should still be pid 1: {procs}"


# ---- exit statuses --------------------------------------------------------------


def test_the_process_server_reports_an_exit_status(sh):
    job = start(sh, "exit 7")
    assert wait_status(sh, job.pid) == 7
    assert server_wait_status(sh, job) == 7 << 8


def test_the_process_server_reports_a_death_by_signal(sh):
    job = start(sh, "pause")
    # SIGKILL cannot be caught, so it does not matter whether the job is in pause() yet.
    assert run(sh, f"kill -KILL {job.pid}") == ""
    assert wait_status(sh, job.pid) == 128 + SIGKILL
    assert server_wait_status(sh, job) == SIGKILL


def test_the_process_server_reports_the_exit_of_init(sh):
    sh.send("exit 3\r")
    sh.expect(f"[{SERVER}] pid 1 (init) exited (wait status {3 << 8:#x})\n")
    # Init exiting still shuts the system down.
    assert sh.wait_exit() == Config.arch.power_off_status


def test_the_kernel_log_never_mentions_a_wait_status(sh):
    # Every way a process can end: exit, a signal, and init itself.
    job = start(sh, "exit 1")
    assert wait_status(sh, job.pid) == 1
    job = start(sh, "pause")
    run(sh, f"kill -KILL {job.pid}")
    assert wait_status(sh, job.pid) == 128 + SIGKILL
    sh.send("exit\r")
    assert sh.wait_exit() == Config.arch.power_off_status
    leaks = kernel_wait_statuses(sh)
    assert not leaks, f"the kernel knows about wait statuses: {leaks}"


# ---- zombies --------------------------------------------------------------------


def test_zombies_do_not_take_up_kernel_process_slots(sh):
    # Each job exits while the shell waits at its next prompt. Nobody reaps it, so it stays a
    # zombie. The kernel has room for KERNEL_PROCESS_SLOTS processes. init, the console server
    # and the process server take three of them, so these zombies cannot all be kernel processes.
    jobs = [start(sh, f"exit {n}") for n in range(1, KERNEL_PROCESS_SLOTS + 1)]
    # Each zombie keeps its own exit status until it is waited for.
    for n, job in enumerate(jobs, 1):
        assert wait_status(sh, job.pid) == n, f"job {n} (pid {job.pid})"
