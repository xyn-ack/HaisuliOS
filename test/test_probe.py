# SPDX-License-Identifier: GPL-2.0
"""Probe tests: process and signal mechanisms that the shell cannot reach.

The shell only catches signals without SA_RESTART, never faults, never ends a task behind the
process server's back and never runs into the scheduling races of the process server. The test
program /bin/probe_test (`user/probe_test`) does: `probe_test CASE` runs one case, prints `ok` or
`FAIL` for each of its checks and then `CASE: passed` or `CASE: failed`, and exits with status 0
only if every check passed. See `user/probe_test/src/main.rs` for what each case does.
"""
import re

from haisuli import PROMPT, Timeout

# The kernel and the process server share the console, so their lines can appear anywhere.
LOG_LINE = re.compile(r"\[(?:kernel|proc)\] [^\n]*\n")


def run(sh, line, timeout=None):
    """Like `sh.run()`, but ignores log lines."""
    sh.send(line + "\r")
    chunk = LOG_LINE.sub("", sh.expect(PROMPT, timeout))[: -len(PROMPT)]
    return chunk.partition("\n")[2]


def probe(sh, *args, timeout=None):
    """Runs `probe_test ARGS...` and checks that the case passed; returns its output."""
    case = args[0]
    command = " ".join(("probe_test",) + args)
    try:
        out = run(sh, command, timeout)
    except Timeout:
        raise AssertionError(f"`{command}` did not finish: something sleeps forever")
    code = run(sh, "echo $?")
    assert "FAIL" not in out and f"{case}: passed\n" in out, f"`{command}` failed:\n{out}"
    assert code == "0\n", f"`{command}` exited with status {code!r}:\n{out}"
    return out


def test_sa_restart_makes_an_interrupted_wait_again(sh):
    probe(sh, "restart")


def test_without_sa_restart_an_interrupted_wait_fails_with_eintr(sh):
    probe(sh, "eintr")


def test_a_cpu_fault_kills_the_process_with_sigsegv(sh):
    sh.allow("[user] #")  # The kernel reports the fault of the child, which is the point.
    since = len(sh.transcript)
    probe(sh, "fault")
    faults = re.findall(r"\[user\] #14 \(page fault\)", sh.transcript[since:])
    assert len(faults) == 1, f"expected the child's page fault only: {sh.transcript[since:]!r}"


def test_a_task_that_ends_itself_is_reported_as_exited(sh):
    # `task_exit` bypasses the process server: the kernel tells it.
    probe(sh, "task-exit")


def test_two_caught_signals_both_run_their_handlers(sh):
    # The kernel holds one upcall per task: the second handler runs when the first returns.
    probe(sh, "two-signals")


def test_a_signal_for_a_queued_pause_ends_it(sh):
    # The process server turns the signal into an upcall while the pause request is still queued;
    # the pause must not go to sleep then. Without that, this case never finishes.
    probe(sh, "pause-race")


def test_execve_keeps_and_resets_what_posix_says(sh):
    probe(sh, "exec")


def test_many_execve_in_a_row_leak_nothing(sh):
    # Every execve builds a new address space (about 35 page frames here) and frees the old one:
    # 1000 of them would use more than the 32000 free frames if the old ones leaked.
    probe(sh, "exec-loop", "1000", timeout=60)
