# SPDX-License-Identifier: GPL-2.0
"""SMP and thread tests: more than one CPU, and more than one thread per process.

These tests were written before the kernel supported either (test first), and now pass. They are
explicit tests: `make check` skips them, and

    make check TEST_EXPLICIT="smp"

runs them (and only them). Each test boots a machine with the number of CPUs it needs (QEMU's
`-smp`, see `haisuli.cpus`). Removing `EXPLICIT` makes them ordinary tests of `make check`.

The kernel used to run on one CPU, and a task used to be an address space with exactly one
thread. The tests check what a user, or a program, can see:

* The kernel starts every CPU at boot and logs how many run, as `[kernel] 4 CPUs online` (or
  `[kernel] 1 CPU online`), before the first prompt. `/bin/nproc` prints that number, as in GNU
  coreutils.
* A job that keeps a CPU busy without ever making a system call takes that CPU, but not the
  machine: the shell, the console server and the process server go on, on the other CPUs. Killing
  the job frees its CPU, although the job never enters the kernel by itself.
* Programs keep working when they run on several CPUs at once.
* Threads: the test program `/bin/smp_test` (`user/smp_test`) starts them with the Linux calls
  that a musl port needs: `clone` with `CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND |
  CLONE_THREAD`, `exit` (of one thread), `gettid` and `getcpu`. The threads of a process share its
  memory and its pid, run at the same time on different CPUs, and end with it: when it exits, is
  killed, faults or runs `execve`. `fork` copies only the thread that calls it, and a signal for
  the process ends the `pause` of a thread that waits in it.

Nothing here needs preemption: every test leaves at least one CPU to the shell and the servers.
So the tests hold whether or not the kernel preempts (it does).

Some tests pass on one CPU already (`test_*_side_by_side`, `test_*_at_once`): they become real
tests of SMP once other CPUs run tasks too, and guard the kernel's locking then. One case of
`probe_test` is left out of them on purpose: `pause-race` makes a race of one CPU's cooperative
scheduling happen, which several CPUs need not reproduce.
"""
import contextlib
import re

from haisuli import PROMPT, Timeout, cpus

# Run by `make check TEST_EXPLICIT="smp"` only (see run_tests.py).
EXPLICIT = "smp"

SIGKILL = 9

# The kernel and the process server log to the same serial line as the console server, so their
# lines can appear anywhere in the output, even in the middle of an echoed command line.
LOG_LINE = re.compile(r"\[(?:kernel|proc)\] [^\n]*\n")

# The kernel's report of a process's CPU exception: a line after an empty one, and indented
# details. A process that faults on another CPU can report in the middle of another command.
FAULT_REPORT = re.compile(r"\n?\[user\] #\S+ [^\n]*\n(?: {4}[^\n]*\n)*")

# Job status notices ("[1]+ Done  exit 7") that the shell may print before a prompt, maybe after
# an empty line. Unlike the "[1] 3" of `cmd &`, the job number is not followed by a pid.
JOB_NOTICE = re.compile(r"^\n?\[\d+\][+-]? *[^\d\s][^\n]*\n", re.M)

# What `smp_test spin` prints before it starts to keep its CPU busy.
SPINNING = "smp_test: spinning\n"

# The `probe_test` cases that do not depend on the order in which tasks run (see above).
ORDER_INDEPENDENT_PROBES = ("restart", "eintr", "fault", "task-exit", "two-signals", "exec")


class Job:
    """A background job: its pid, and where the console transcript stood when it started."""

    def __init__(self, pid, since):
        self.pid = pid
        self.since = since


# ---- helpers ------------------------------------------------------------------


def run(sh, line, timeout=None):
    """Like `sh.run()`, but ignores log lines, fault reports and job status notices."""
    sh.send(line + "\r")
    chunk = FAULT_REPORT.sub("", LOG_LINE.sub("", sh.expect(PROMPT, timeout)))[: -len(PROMPT)]
    return JOB_NOTICE.sub("", chunk.partition("\n")[2])


def status(sh):
    """The shell's `$?`."""
    out = run(sh, "echo $?")
    assert re.fullmatch(r"\d+\n", out), f"`echo $?` printed {out!r}"
    return int(out)


@contextlib.contextmanager
def shell_responds(what):
    """Turns the shell not answering into a failure that says why that matters."""
    try:
        yield
    except Timeout as e:
        raise AssertionError(
            f"the shell stopped responding {what}: busy jobs must leave the other CPUs to the "
            f"rest of the system") from e


def start(sh, command):
    """Starts `command &` and returns the job."""
    since = len(sh.transcript)
    out = run(sh, f"{command} &")
    m = re.search(r"^\[\d+\] (\d+)$", out, re.M)
    assert m, f"`{command} &` should print the job number and pid ('[1] 4'), printed {out!r}"
    job = Job(int(m[1]), since)
    job.early_output = out  # What the job printed before the shell's prompt (on another CPU).
    return job


def wait_status(sh, pid, timeout=None):
    """Runs `wait pid` and returns its status."""
    out = run(sh, f"wait {pid}", timeout)
    assert out == "", f"`wait {pid}` printed {out!r}"
    return status(sh)


def start_spinning(sh):
    """Starts a job that keeps a CPU busy, and waits until it does.

    The job says so before or after the shell's next prompt (it runs on another CPU); after it,
    that is consumed here, so that it does not count as the output of the next command.
    """
    job = start(sh, "smp_test spin")
    if SPINNING not in job.early_output:
        sh.expect(SPINNING)
    return job


def check_shell_responds(sh, rounds=5):
    for i in range(rounds):
        out = run(sh, f"echo round {i}")
        assert out == f"round {i}\n", f"`echo round {i}` printed {out!r}"


def kill_job(sh, job):
    """Kills a job with SIGKILL and reaps it."""
    assert run(sh, f"kill -KILL {job.pid}") == ""
    code = wait_status(sh, job.pid)
    assert code == 128 + SIGKILL, f"`wait {job.pid}` of a killed job gave status {code}"


def smp_test(sh, *args, timeout=None, hang=""):
    """Runs `smp_test ARGS...` and checks that the case passed; returns its output.

    `hang` says what it means if the case never finishes.
    """
    case = args[0]
    command = " ".join(("smp_test",) + args)
    try:
        out = run(sh, command, timeout)
    except Timeout:
        raise AssertionError(f"`{command}` did not finish{hang}")
    code = status(sh)
    assert "FAIL" not in out and f"{case}: passed\n" in out, f"`{command}` failed:\n{out}"
    assert code == 0, f"`{command}` exited with status {code}:\n{out}"
    return out


# What it means when a case that ends a process with threads never finishes (see `smp_test`'s
# `cpus_are_free`).
THREADS_LEFT_BEHIND = ": a thread of the process that ended may still keep a CPU busy"


# ---- the CPUs -----------------------------------------------------------------


def check_cpus_online(sh, n):
    line = "[kernel] 1 CPU online" if n == 1 else f"[kernel] {n} CPUs online"
    assert re.search(rf"^{re.escape(line)}$", sh.boot_output, re.M), \
        f"the boot log should say {line!r}:\n{sh.boot_output}"


@cpus(1)
def test_the_kernel_reports_one_cpu(sh):
    check_cpus_online(sh, 1)


@cpus(2)
def test_the_kernel_starts_two_cpus(sh):
    check_cpus_online(sh, 2)


@cpus(4)
def test_the_kernel_starts_four_cpus(sh):
    check_cpus_online(sh, 4)


def check_nproc(sh, n):
    out = run(sh, "nproc")
    assert out == f"{n}\n", f"`nproc` should print {n}, printed {out!r}"


@cpus(1)
def test_nproc_prints_one_on_one_cpu(sh):
    check_nproc(sh, 1)


@cpus(4)
def test_nproc_prints_the_number_of_cpus(sh):
    check_nproc(sh, 4)


# ---- jobs on several CPUs -----------------------------------------------------


@cpus(2)
def test_a_job_that_keeps_a_cpu_busy_does_not_stall_the_shell(sh):
    job = start_spinning(sh)
    with shell_responds("while a job kept one of two CPUs busy"):
        check_shell_responds(sh)
        kill_job(sh, job)


@cpus(4)
def test_jobs_can_keep_every_cpu_but_one_busy(sh):
    with shell_responds("while jobs kept three of four CPUs busy"):
        jobs = [start_spinning(sh) for _ in range(3)]
        check_shell_responds(sh)
        for job in jobs:
            kill_job(sh, job)


@cpus(2)
def test_killing_a_busy_job_frees_its_cpu(sh):
    # The job never enters the kernel, so the kernel must interrupt its CPU to end it. If the CPU
    # stayed busy, the next job would take the last one.
    for i in range(3):
        with shell_responds(f"after {i} busy jobs had been killed"):
            job = start_spinning(sh)
            check_shell_responds(sh, rounds=2)
            kill_job(sh, job)


def run_side_by_side(sh, commands, timeout):
    """Starts every command as a job at once, and checks that each exits with status 0.

    Each job writes to a file of its own: on the terminal, the output of jobs would mix with that
    of the shell's commands.
    """
    jobs = [(command, f"/job{i}.out", start(sh, f"{command} > /job{i}.out 2>&1"))
            for i, command in enumerate(commands)]
    for command, out, job in jobs:
        code = wait_status(sh, job.pid, timeout)
        assert code == 0, f"`{command}` exited with status {code}:\n{run(sh, f'cat {out}')}"


@cpus(4)
def test_programs_fork_and_exec_at_once(sh):
    # Each job runs execve 100 times in a row, at the same time as the others.
    run_side_by_side(sh, ["probe_test exec-loop 100"] * 4, timeout=60)


@cpus(4)
def test_probe_cases_pass_side_by_side(sh):
    sh.allow("[user] #")  # The `fault` case makes a child fault, on purpose.
    run_side_by_side(sh, [f"probe_test {case}" for case in ORDER_INDEPENDENT_PROBES], timeout=30)


# ---- threads ------------------------------------------------------------------


@cpus(1)
def test_threads_run_on_one_cpu(sh):
    # Threads do not need SMP: on one CPU they take turns.
    smp_test(sh, "threads")


@cpus(4)
def test_threads_run_on_four_cpus(sh):
    smp_test(sh, "threads")


@cpus(2)
def test_two_threads_run_at_once_on_two_cpus(sh):
    smp_test(sh, "parallel", "2", hang=": the threads never all ran at the same time")


@cpus(4)
def test_four_threads_run_at_once_on_four_cpus(sh):
    smp_test(sh, "parallel", "4", hang=": the threads never all ran at the same time")


@cpus(2)
def test_many_threads_in_a_row_leak_nothing(sh):
    smp_test(sh, "thread-loop", "1000", timeout=60)


@cpus(4)
def test_threads_make_system_calls_at_once(sh):
    smp_test(sh, "syscalls", "4")


@cpus(4)
def test_exit_ends_every_thread(sh):
    smp_test(sh, "exit", "4", hang=THREADS_LEFT_BEHIND)


@cpus(4)
def test_sigkill_ends_every_thread(sh):
    smp_test(sh, "kill", "4", hang=THREADS_LEFT_BEHIND)


@cpus(2)
def test_a_fault_in_one_thread_kills_the_whole_process(sh):
    sh.allow("[user] #")  # The kernel reports the fault, which is the point.
    since = len(sh.transcript)
    smp_test(sh, "fault", "2", hang=THREADS_LEFT_BEHIND)
    faults = re.findall(r"\[user\] #\S+ \(page fault\)", sh.transcript[since:])
    assert len(faults) == 1, f"expected the thread's page fault only: {sh.transcript[since:]!r}"


@cpus(4)
def test_execve_ends_the_other_threads(sh):
    smp_test(sh, "exec", "4", hang=THREADS_LEFT_BEHIND)


@cpus(4)
def test_fork_copies_only_the_calling_thread(sh):
    smp_test(sh, "fork")


@cpus(2)
def test_a_signal_ends_the_pause_of_the_thread_that_waits(sh):
    # The process server's POSIX layer: a signal for the process goes to a thread that waits in
    # `pause`, whose handler runs in that thread.
    smp_test(sh, "signals", hang=": the pause in a thread never ended")
