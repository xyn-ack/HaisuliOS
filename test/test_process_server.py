# SPDX-License-Identifier: GPL-2.0
"""Process server tests: POSIX processes are managed by a user-space server, not by the kernel.

These tests describe the process server before it exists, so they are expected to fail until it
is implemented. They form the explicit target `process-server`, which `make check` leaves out:
run them with `make check TEST_EXPLICIT="process-server"`.

A true microkernel knows nothing about POSIX processes. `fork`, `exit`, `wait4`, `getpid` and
`getppid` (process ids, parents and children, exit statuses and zombies) move into a process
server, a boot module like the console server, and programs reach it over IPC through the runtime
(`user/rt`), as they would through a libc. What the tests expect:

* The kernel starts the process server as a boot module called `proc`, and `ps` lists it as a
  live process.
* The kernel's system call table no longer has the Linux-numbered `getpid` (39), `getppid` (110),
  `wait4` (61), `fork` (57) and `exit` (60): they fail with `-ENOSYS` (-38).
* The runtime's `fork`, `exit`, `waitpid`, `getpid` and `getppid` still work, for every program.

The shell's view of processes must not change either; the rest of the test suite (`make check`)
covers that.

The kernel's side is checked by a probe, `user/proctest`, which these tests boot as an extra boot
module (there is no `exec`). It makes those system calls directly and through the runtime, and
reports what happened on the kernel debug log, one `[proctest] what = value` line each, ending
with `[proctest] done = yes`. It only reports; the tests below decide what is right.
"""
import re
import time

from haisuli import PROMPT

EXPLICIT = "process-server"
EXTRA_MODULES = ("build/proctest",)

# The boot module name of the process server.
SERVER = "proc"

# Linux errno values, as in abi/src/lib.rs.
ECHILD = 10
ENOSYS = 38

# The code the probe's child exits with (CHILD_EXIT_CODE in user/proctest), and its wait status.
CHILD_EXIT_CODE = 42
CHILD_WAIT_STATUS = CHILD_EXIT_CODE << 8

# Linux x86_64 system call numbers of the calls the kernel must not implement any more.
SYSCALLS = {"getpid": 39, "getppid": 110, "wait4": 61, "fork": 57, "exit": 60}

# The kernel debug log shares the console, so a probe line can start in the middle of a line.
PROBE_LINE = re.compile(r"\[proctest\] ([^=\n]+?) = ([^\n]*)\n")
PROBE_DONE = "[proctest] done = yes\n"

KERNEL_LINE = re.compile(r"\[kernel\] [^\n]*\n")


# ---- helpers ------------------------------------------------------------------


def probe(sh):
    """Waits for the probe to finish and returns its report as {what: value}."""
    deadline = time.monotonic() + sh.config.timeout
    while PROBE_DONE not in sh.transcript:
        if time.monotonic() > deadline:
            started = "[proctest]" in sh.transcript
            raise AssertionError(
                "the probe did not finish" if started else "the probe (user/proctest) never ran")
        time.sleep(0.05)
    return dict(PROBE_LINE.findall(sh.transcript))


def number(report, what, base=10):
    """The probe's report on `what`, which must be a number."""
    value = report.get(what)
    assert value is not None, f"the probe did not report {what!r}: {report}"
    try:
        return int(value, base)
    except ValueError:
        raise AssertionError(f"{what}: {value}") from None


def assert_not_in_kernel(sh, call):
    """Checks that the kernel answered the probe's direct `call` with -ENOSYS."""
    report = probe(sh)
    result = number(report, f"kernel {call}")
    assert result == -ENOSYS, (
        f"the kernel still implements {call}(): system call {SYSCALLS[call]} returned {result}, "
        f"not -ENOSYS ({-ENOSYS})")


def ps(sh):
    """The `ps` listing as a list of {column: value} rows."""
    sh.send("ps\r")
    out = KERNEL_LINE.sub("", sh.expect(PROMPT))[: -len(PROMPT)].partition("\n")[2]
    lines = out.splitlines()
    assert lines, "`ps` printed nothing"
    header = lines[0].split()
    return [dict(zip(header, line.split())) for line in lines[1:] if line.strip()]


# ---- the process server is a boot module ----------------------------------------


def test_the_kernel_starts_the_process_server(sh):
    assert re.search(rf"^\[kernel\] started pid \d+ \({SERVER}\): \d+ KiB image$",
                     sh.boot_output, re.M), "the process server was not started at boot"


def test_ps_lists_the_process_server(sh):
    rows = [row for row in ps(sh) if row.get("NAME") == SERVER]
    assert len(rows) == 1, f"`ps` should list one {SERVER!r} process: {rows}"
    assert rows[0]["STAT"][0] in "RS", f"the process server should be alive: {rows[0]}"


# ---- the kernel knows nothing about POSIX processes -----------------------------


def test_the_kernel_has_no_getpid(sh):
    assert_not_in_kernel(sh, "getpid")


def test_the_kernel_has_no_getppid(sh):
    assert_not_in_kernel(sh, "getppid")


def test_the_kernel_has_no_wait4(sh):
    assert_not_in_kernel(sh, "wait4")


def test_the_kernel_has_no_fork(sh):
    assert_not_in_kernel(sh, "fork")


def test_the_kernel_has_no_exit(sh):
    # The probe tries it in a child. If the kernel's `exit` works, the child is gone before it can
    # report, and only its wait status is left.
    report = probe(sh)
    assert "kernel exit" in report, (
        "the kernel still implements exit(): the child calling it exited with wait status "
        f"{report.get('kernel exit child status', report.get('kernel exit child'))}")
    assert_not_in_kernel(sh, "exit")


# ---- the runtime still has POSIX processes --------------------------------------


def test_a_program_forks_waits_and_exits_through_the_runtime(sh):
    report = probe(sh)
    parent, child = number(report, "pid"), number(report, "fork")
    assert child > 0 and child != parent, f"fork() returned {child} to pid {parent}"
    assert number(report, "child getpid") == child
    assert number(report, "child getppid") == parent
    assert number(report, "wait pid") == child
    assert number(report, "wait status", 16) == CHILD_WAIT_STATUS
    # Reaped, so no longer a child.
    assert report.get("second wait") == f"error {ECHILD}", report.get("second wait")
