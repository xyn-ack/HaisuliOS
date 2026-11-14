# SPDX-License-Identifier: GPL-2.0
"""POSIX process tests: signals, process groups and sessions, waitpid semantics.

POSIX processes are implemented in user space, by the process server (`user/proc`); the kernel
only provides the tasks they run in. These tests check what processes look like from the shell.

There is no `exec` yet, so every command is a shell built-in. The tests drive the processes
through the init shell with standard POSIX shell features, which the shell gains together with
the feature:

* `cmd &` runs `cmd` in a forked child, in a new process group of the shell's session, and
  prints the job number and the child's pid: `[1] 3`.
* `$?` expands to the exit status of the last command; 128 + n if signal n killed it.
* `wait [pid...]` waits for the given children (all children without operands) and sets `$?` to
  the status of the last one; 127 if it is not a child of the shell (or was already waited for).
* `kill [-s name | -name | -n] pid...` sends a signal (default SIGTERM) to processes, or to a
  process group for a negative pid. Signal 0 only checks that the target exists.
* `exit n` in an asynchronous list (`exit 3 &`) exits the child with status n.
* `ps` lists processes under a header with (at least) PID, PPID, PGID, SID and STAT; STAT starts
  with S for a process blocked in pause(), T for a stopped one and Z for a zombie.
* `pause` blocks until a signal is delivered (pause(2)). In the foreground it runs in a child in
  the terminal's foreground process group, so Ctrl-C sends it SIGINT.
* `trap 'command' SIG...` runs the command when the shell catches a signal, with a real signal
  handler; `trap - SIG` restores the default action and `trap '' SIG` ignores the signal. `$$`
  is the shell's pid, and single quotes make one word of their contents.

Signal numbers are the Linux x86_64 ones, like the rest of the ABI. Where the process server logs
the exit of a process, its POSIX wait status is checked too.
"""
import re
import time

from haisuli import PROMPT, Timeout

SIGHUP = 1
SIGINT = 2
SIGKILL = 9
SIGUSR1 = 10
SIGTERM = 15

# The exit status of "command not found", and of `wait` for a pid that is not a child.
STATUS_UNKNOWN = 127

# The kernel and the process server (which logs every exit) write to the console they share with
# the console server, so their lines can appear anywhere in the output: after a prompt, even in the
# middle of an echoed command line.
LOG_LINE = re.compile(r"\[(?:kernel|proc)\] [^\n]*\n")

# Job status notices ("[1]+ Terminated  pause") that a shell may print before a prompt, maybe
# after an empty line. Unlike the "[1] 3" of `cmd &`, the job number is not followed by a pid.
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
    assert re.fullmatch(r"\d+\n", out), f"the shell does not expand $?: `echo $?` printed {out!r}"
    return int(out)


def start(sh, command):
    """Starts `command &` and returns the job."""
    since = len(sh.transcript)
    out = run(sh, f"{command} &")
    m = re.search(r"^\[\d+\] (\d+)$", out, re.M)
    assert m, f"`{command} &` should print the job number and pid ('[1] 3'), printed {out!r}"
    return Job(int(m[1]), since)


def start_pause(sh):
    """Starts `pause &` and waits until the job is blocked in pause().

    Until the child gets there, it may still have the shell's signal dispositions.
    """
    job = start(sh, "pause")
    wait_for_stat(sh, job.pid, lambda stat: stat.startswith("S"), "blocked in pause()")
    return job


def wait_status(sh, pid):
    """Runs `wait pid` and returns its status."""
    out = run(sh, f"wait {pid}")
    code = status(sh)
    assert code == STATUS_UNKNOWN or out == "", f"`wait {pid}` printed {out!r}"
    return code


def is_alive(sh, pid):
    """Whether `kill -0 pid` succeeds."""
    out = run(sh, f"kill -0 {pid}")
    code = status(sh)
    assert code != 0 or out == "", f"`kill -0 {pid}` succeeded but printed {out!r}"
    return code == 0


def logged_wait_status(sh, job):
    """Waits until `job` has exited and returns the wait status that the process server logged.

    Pids are reused, so only the log since the job started counts.
    """
    pattern = re.compile(
        rf"\[proc\] pid {job.pid} \(\S+\) exited \(wait status (0x[0-9a-f]+)\)$", re.M)
    deadline = time.monotonic() + sh.config.timeout
    while True:
        m = pattern.search(sh.transcript, job.since)
        if m:
            return int(m[1], 16)
        assert time.monotonic() < deadline, f"no exit of pid {job.pid} in the log"
        time.sleep(0.05)


def ps(sh):
    """The `ps` listing as {pid: {column: value}}."""
    out = run(sh, "ps")
    lines = out.splitlines()
    for i, line in enumerate(lines):
        header = line.split()
        if {"PID", "PPID", "PGID", "SID", "STAT"} <= set(header):
            break
    else:
        raise AssertionError(f"`ps` should print a PID PPID PGID SID STAT header, printed {out!r}")
    procs = {}
    for line in lines[i + 1:]:
        if not line.strip():
            continue
        row = dict(zip(header, line.split()))
        assert row.get("PID", "").isdigit(), f"bad `ps` line {line!r} in {out!r}"
        procs[int(row["PID"])] = row
    return procs


def wait_for_stat(sh, pid, predicate, what):
    """Polls `ps` until the STAT of `pid` satisfies `predicate`; returns the `ps` row."""
    deadline = time.monotonic() + sh.config.timeout
    while True:
        row = ps(sh).get(pid)
        assert row is not None, f"pid {pid} is gone, but it should be {what}"
        if predicate(row["STAT"]):
            return row
        assert time.monotonic() < deadline, f"pid {pid} should be {what}; ps shows {row}"
        time.sleep(0.1)


def assert_reaped(sh, jobs):
    """Checks that none of `jobs` is left as a zombie.

    Pids are reused (the lowest free one first), so a reaped job's pid may already belong to
    another process, such as `ps` itself.
    """
    procs = ps(sh)
    for job in jobs:
        row = procs.get(job.pid)
        assert row is None or not row["STAT"].startswith("Z"), f"pid {job.pid} not reaped: {row}"


def interrupt(sh, line):
    """Runs `line` in the foreground, which must block, and interrupts it with Ctrl-C.

    Returns the output up to the next prompt (without log lines).
    """
    sh.send(line + "\r")
    try:
        early = sh.expect(PROMPT, timeout=1.0)
    except Timeout:
        pass  # Still running, as it should.
    else:
        raise AssertionError(f"`{line}` returned without being interrupted: {early!r}")
    # The shell hands the terminal to the job's process group after forking it. A Ctrl-C that
    # arrives before that goes to the shell (which survives it), so retry for a while.
    for _ in range(5):
        sh.send("\x03")
        try:
            return LOG_LINE.sub("", sh.expect(PROMPT, timeout=3.0))[: -len(PROMPT)]
        except Timeout:
            assert "init exited" not in sh.transcript, "Ctrl-C killed the shell"
    raise AssertionError(f"Ctrl-C did not interrupt `{line}`")


# ---- exit status and waitpid semantics ----------------------------------------


def test_exit_status_of_the_last_command_is_in_dollar_question(sh):
    assert run(sh, "echo hi") == "hi\n"
    assert status(sh) == 0
    run(sh, "nosuchcommand")
    assert status(sh) == STATUS_UNKNOWN


def test_wait_returns_the_exit_status_of_a_background_job(sh):
    job = start(sh, "exit 7")
    assert job.pid > 3, "pids 1 to 3 are init, the process server and the console server"
    assert wait_status(sh, job.pid) == 7
    assert logged_wait_status(sh, job) == 7 << 8


def test_wait_picks_the_given_child_out_of_several(sh):
    first = start(sh, "exit 3")
    second = start(sh, "exit 5")
    assert first.pid != second.pid
    # The second child first: the first one's status must be kept until it is waited for.
    assert wait_status(sh, second.pid) == 5
    assert wait_status(sh, first.pid) == 3


def test_wait_does_not_block_on_other_running_children(sh):
    sleeper = start_pause(sh)
    quitter = start(sh, "exit 4")
    assert wait_status(sh, quitter.pid) == 4
    assert is_alive(sh, sleeper.pid)
    run(sh, f"kill -KILL {sleeper.pid}")
    assert wait_status(sh, sleeper.pid) == 128 + SIGKILL


def test_a_child_can_be_waited_for_only_once(sh):
    job = start(sh, "exit 2")
    assert wait_status(sh, job.pid) == 2
    assert wait_status(sh, job.pid) == STATUS_UNKNOWN  # Reaped: no longer a child (ECHILD).
    assert_reaped(sh, [job])


def test_wait_for_a_process_that_is_not_a_child_fails(sh):
    assert wait_status(sh, 2) == STATUS_UNKNOWN  # The process server, a child of no one.
    assert wait_status(sh, 3) == STATUS_UNKNOWN  # The console server, which it started.
    assert wait_status(sh, 1) == STATUS_UNKNOWN  # The shell itself.
    assert wait_status(sh, 15) == STATUS_UNKNOWN  # No such process.
    assert run(sh, "echo ok") == "ok\n"


def test_wait_without_operands_reaps_every_child(sh):
    jobs = [start(sh, "exit 1"), start(sh, "exit 2")]
    assert run(sh, "wait") == ""
    assert status(sh) == 0
    assert_reaped(sh, jobs)
    assert [logged_wait_status(sh, job) for job in jobs] == [1 << 8, 2 << 8]


# ---- signals ------------------------------------------------------------------


def test_kill_sends_sigterm_by_default(sh):
    job = start_pause(sh)
    assert run(sh, f"kill {job.pid}") == ""
    assert status(sh) == 0
    assert wait_status(sh, job.pid) == 128 + SIGTERM
    # The POSIX wait status of a process killed by signal n is n.
    assert logged_wait_status(sh, job) == SIGTERM


def test_kill_sends_the_signal_given_by_name_or_number(sh):
    for option, signal in [
        ("-s HUP", SIGHUP),
        ("-INT", SIGINT),
        ("-9", SIGKILL),
        ("-s USR1", SIGUSR1),
        ("-15", SIGTERM),
    ]:
        job = start_pause(sh)
        assert run(sh, f"kill {option} {job.pid}") == "", option
        assert wait_status(sh, job.pid) == 128 + signal, option
        assert logged_wait_status(sh, job) == signal, option


def test_kill_0_only_checks_that_the_process_exists(sh):
    job = start_pause(sh)
    assert is_alive(sh, job.pid)
    assert is_alive(sh, job.pid)  # Signal 0 is not delivered, so it did no harm.
    run(sh, f"kill {job.pid}")
    assert wait_status(sh, job.pid) == 128 + SIGTERM
    assert not is_alive(sh, job.pid)


def test_kill_of_a_nonexistent_process_fails(sh):
    out = run(sh, "kill 15")
    assert "No such process" in out, out  # ESRCH.
    assert status(sh) != 0


def test_kill_rejects_an_unknown_signal(sh):
    job = start_pause(sh)
    out = run(sh, f"kill -s NOSUCHSIGNAL {job.pid}")
    assert out != "", "no error message"
    assert status(sh) != 0
    out = run(sh, f"kill -99 {job.pid}")
    assert out != "", "no error message"
    assert status(sh) != 0
    assert is_alive(sh, job.pid)


# ---- stopping and continuing --------------------------------------------------


def test_sigstop_stops_and_sigcont_resumes(sh):
    job = start_pause(sh)
    run(sh, f"kill -STOP {job.pid}")
    wait_for_stat(sh, job.pid, lambda stat: stat.startswith("T"), "stopped")
    run(sh, f"kill -CONT {job.pid}")
    wait_for_stat(sh, job.pid, lambda stat: not stat.startswith("T"), "running again")
    # SIGCONT does not end pause(): no handler ran.
    assert is_alive(sh, job.pid)
    run(sh, f"kill {job.pid}")
    assert wait_status(sh, job.pid) == 128 + SIGTERM


def test_sigkill_terminates_a_stopped_process(sh):
    job = start_pause(sh)
    run(sh, f"kill -STOP {job.pid}")
    wait_for_stat(sh, job.pid, lambda stat: stat.startswith("T"), "stopped")
    run(sh, f"kill -KILL {job.pid}")
    # Checked before `wait`, which in a job control shell may return 128 + SIGSTOP at once for a
    # job that the shell has not seen die yet.
    assert logged_wait_status(sh, job) == SIGKILL
    assert wait_status(sh, job.pid) == 128 + SIGKILL


def test_sigterm_to_a_stopped_process_waits_for_sigcont(sh):
    job = start_pause(sh)
    run(sh, f"kill -STOP {job.pid}")
    wait_for_stat(sh, job.pid, lambda stat: stat.startswith("T"), "stopped")
    run(sh, f"kill -TERM {job.pid}")
    # Only SIGKILL (and SIGCONT) act on a stopped process; SIGTERM stays pending until it continues.
    row = ps(sh).get(job.pid)
    assert row is not None and row["STAT"].startswith("T"), f"not stopped any more: {row}"
    assert f"[proc] pid {job.pid} " not in sh.transcript[job.since:]
    run(sh, f"kill -CONT {job.pid}")
    assert logged_wait_status(sh, job) == SIGTERM  # Before `wait`, as above.
    assert wait_status(sh, job.pid) == 128 + SIGTERM


# ---- process groups and sessions ----------------------------------------------


def test_a_background_job_is_a_process_group_in_the_shell_session(sh):
    job = start_pause(sh)
    procs = ps(sh)
    shell = procs[1]
    assert (shell["PGID"], shell["SID"]) == ("1", "1"), f"init should lead its session: {shell}"
    child = procs[job.pid]
    assert child["PPID"] == "1", child
    assert child["PGID"] == str(job.pid), f"the job should lead its own process group: {child}"
    assert child["SID"] == "1", f"the job should be in the shell's session: {child}"


def test_kill_of_a_negative_pid_signals_the_whole_process_group(sh):
    target = start_pause(sh)
    bystander = start_pause(sh)
    assert run(sh, f"kill -TERM -{target.pid}") == ""  # Process group target.pid.
    assert status(sh) == 0
    assert wait_status(sh, target.pid) == 128 + SIGTERM
    assert is_alive(sh, bystander.pid)
    assert run(sh, "echo the shell is fine") == "the shell is fine\n"
    out = run(sh, f"kill -TERM -{target.pid}")  # That group is gone now.
    assert "No such process" in out, out
    assert status(sh) != 0


# ---- the terminal's foreground process group ------------------------------------


def test_ctrl_c_interrupts_the_foreground_job(sh):
    since = len(sh.transcript)
    interrupt(sh, "pause")
    assert status(sh) == 128 + SIGINT
    assert re.search(r"\[proc\] pid \d+ \(\S+\) exited \(wait status 0x2\)$",
                     sh.transcript[since:], re.M)
    assert run(sh, "echo still here") == "still here\n"


def test_ctrl_c_at_the_prompt_spares_background_jobs(sh):
    # Between jobs the terminal's foreground group is the shell's, group 1; signalling it must
    # not turn into kill(-1), the broadcast to every process.
    job = start_pause(sh)
    sh.send("\x03")
    assert LOG_LINE.sub("", sh.expect(PROMPT)) == "^C\n" + PROMPT
    assert is_alive(sh, job.pid)
    assert f"[proc] pid {job.pid} " not in sh.transcript[job.since:]


def test_ctrl_c_spares_background_jobs_and_the_shell(sh):
    job = start_pause(sh)
    interrupt(sh, "pause")
    assert status(sh) == 128 + SIGINT
    assert is_alive(sh, job.pid)
    assert run(sh, "echo still here") == "still here\n"


# ---- signal handlers (trap) -----------------------------------------------------


def test_trap_runs_a_command_when_the_shell_catches_the_signal(sh):
    assert run(sh, "trap 'echo caught USR1' USR1") == ""
    assert run(sh, "kill -USR1 $$") == "caught USR1\n"
    # POSIX: a trap action leaves `$?` as it was (here, the status of `kill`).
    assert status(sh) == 0
    assert "[proc] pid 1 " not in sh.transcript


def test_a_handler_catches_the_signal_again_and_again(sh):
    # The signal is blocked while its handler runs; returning from the handler must unblock it.
    run(sh, "trap 'echo caught' USR2")
    for _ in range(3):
        assert run(sh, "kill -USR2 $$") == "caught\n"


def test_trap_dash_restores_the_default_action(sh):
    run(sh, "trap 'echo never' USR1")
    run(sh, "trap - USR1")
    sh.send("kill -USR1 $$\r")
    out = sh.expect(f"[proc] pid 1 (init) exited (wait status {SIGUSR1:#x})\n")
    assert "never" not in out, "the trap ran"


def test_trap_of_an_empty_command_ignores_the_signal(sh):
    run(sh, "trap '' USR1")
    assert run(sh, "kill -USR1 $$") == ""
    assert status(sh) == 0
    assert run(sh, "echo still here") == "still here\n"


def test_kill_and_stop_cannot_be_trapped(sh):
    assert run(sh, "trap 'echo ok' USR1") == ""  # `trap` works for other signals.
    for name in ["KILL", "STOP"]:
        out = run(sh, f"trap 'echo no' {name}")
        assert name in out, f"no error message for {name}: {out!r}"
        assert status(sh) != 0, name


def test_a_signal_from_a_job_runs_the_trap(sh):
    # The job signals the shell whenever the shell next waits: while it prints, or while it waits
    # for input (then the read is interrupted). Either way the trap runs before the next command
    # is read, and the terminal keeps working.
    run(sh, "trap 'echo got USR1' USR1")
    sh.send("kill -USR1 $$ &\r")
    sh.expect("got USR1\n")
    sh.expect(PROMPT)
    assert run(sh, "echo still reading") == "still reading\n"


def test_jobs_do_not_inherit_the_shells_traps(sh):
    run(sh, "trap 'echo caught by the shell' USR1")
    job = start_pause(sh)
    assert run(sh, f"kill -USR1 {job.pid}") == ""  # Neither the shell nor the job ran the trap.
    assert wait_status(sh, job.pid) == 128 + SIGUSR1  # The default action: terminate.


def test_ctrl_c_interrupts_a_waiting_shell(sh):
    job = start_pause(sh)
    interrupt(sh, f"wait {job.pid}")  # The shell itself is blocked in wait4().
    # POSIX: a `wait` interrupted by a signal has a status above 128.
    assert status(sh) == 128 + SIGINT
    assert is_alive(sh, job.pid), "SIGINT went to the shell, not to the job"


def test_trap_on_int_runs_at_ctrl_c(sh):
    run(sh, "trap 'echo trapped INT' INT")
    sh.send("\x03")
    assert LOG_LINE.sub("", sh.expect(PROMPT)) == "^C\ntrapped INT\n" + PROMPT
    job = start_pause(sh)
    out = interrupt(sh, f"wait {job.pid}")
    assert "trapped INT\n" in out
    assert status(sh) == 128 + SIGINT
