# Testing

How HaisuliOS is tested. The [README](../README.md) has the overview;
[ARCHITECTURE.md](ARCHITECTURE.md) describes what the tests exercise.

## Running the tests

```sh
make check                          # build, then run every test
make check TEST_ARGS="echo"         # only tests whose id contains "echo"
make check TEST_ARGS="-v -j 1"      # one at a time, print every transcript
make check TEST_EXPLICIT="process-server"  # only the explicit tests named process-server
make check TEST_EXPLICIT="smp"      # only the SMP and thread tests (see below)
make check TEST_ARGS="--cpus 4"     # every machine gets 4 CPUs, unless its test asks otherwise
python3 test/run_tests.py --list    # list the test ids
```

*Explicit* tests, such as the tests of a feature that is still being written, never run by
default. A test file makes its tests explicit with `EXPLICIT = "name"` at module level. They
run only when `TEST_EXPLICIT` gives that exact name (a substring does not match, unlike
`TEST_ARGS`), and then only they run. `TEST_ARGS` can still narrow them down.

A machine has one CPU unless its test asks for more with the `@cpus(n)` decorator from
`haisuli` (QEMU's `-smp n`). `--cpus N` gives every other machine `N` CPUs, e.g. to run the
whole suite on several CPUs: `make check TEST_ARGS="--cpus 4"` passes too.

A test that asks for `n` CPUs may keep them all busy, so it also holds `n` of the host's CPUs
while it runs (`--cpu-budget`, by default the host's number): the runner never starts more
emulated CPUs that spin than the host has, which would make every test slow and its timeouts
wrong. Machines that get their CPUs from `--cpus` mostly idle, and count as one.

## How the tests work

The tests drive the shell the way a user would. Each test boots its own
QEMU machine with the console on a pipe, types at the prompt, and checks what
appears on the screen. Tests run in parallel, and only the Python 3 standard
library is needed. After every test, the transcript is also checked for
kernel panics, CPU exceptions and user-space panics (a test that causes one on
purpose says so with `sh.allow(marker)`). On failure, the runner prints the
traceback and the full console transcript.

## The test program

What the shell cannot reach, the test program `/bin/probe_test` does:
`probe_test CASE` runs one case (`restart`, `eintr`, `fault`, `task-exit`,
`two-signals`, `pause-race`, `blocked-ignored`, `exec`, `exec-loop N`),
prints `ok` or `FAIL` for each check, and exits with status 0 only if all
passed. `test_probe.py` runs every case.

`/bin/smp_test` does the same for multiple CPUs and threads (`test_smp.py`): `smp_test CASE [N]`,
where `N` is the machine's number of CPUs, runs one case (`spin`, `threads`, `parallel N`,
`thread-loop N`, `syscalls N`, `exit N`, `kill N`, `fault N`, `exec N`, `fork`, `signals`). It
starts threads with the runtime's `thread::spawn`, on the kernel's `clone`.

## The test files

```
test/run_tests.py         runner: discovery, parallel execution, reporting
test/haisuli.py           harness: Machine (boot, send, expect, run, run_raw)
test/test_boot.py         boot messages, /etc/motd shown before the first prompt
test/test_commands.py     help, echo, ls, cat, unknown commands, odd input
test/test_line_editing.py echo, Backspace, Ctrl-U/C, escape sequences, long lines
test/test_exit.py         exit and Ctrl-D power the machine off
test/test_exec.py         programs from the file system: execve, /bin, /sbin/console
test/test_probe.py        /bin/probe_test: SA_RESTART, faults, task_exit, back-to-back signals,
                          the queued-pause race, blocked signals that are ignored when sent,
                          what execve keeps, 1000 execs in a row
test/test_process.py      signals, process groups/sessions, waitpid semantics; jobs signalled
                          the moment they start
test/test_process_server.py  explicit ("process-server"): process management lives in the
                          process server (pids, exit statuses, zombies), not the kernel
test/test_shell_redirection.py  >, >>, <, 2> and 2>&1, backed by the kernel's files
test/test_smp.py          explicit ("smp"): written before SMP and threads existed. The CPUs are
                          started and counted (boot log, nproc); jobs that keep CPUs busy leave
                          the shell one; threads run at once on different CPUs and end with
                          their process (exit, kill, fault, execve); fork copies one thread;
                          a signal ends the pause of the thread that waits
```

## Writing a test

A test is a function `test_*(sh)` in a `test/test_*.py` file. It receives a
machine waiting at its first prompt: `sh.run("echo hi")` types a command and
returns its output (`"hi\n"`), `sh.run_raw(keys)` also returns the echoed
line, and `sh.send()` / `sh.expect()` give full control. A test that needs several CPUs says so
with `@cpus(n)` (`from haisuli import cpus`).
