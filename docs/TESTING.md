# Testing

How HaisuliOS is tested. The [README](../README.md) has the overview;
[ARCHITECTURE.md](ARCHITECTURE.md) describes what the tests exercise.

## Running the tests

```sh
make check                          # build, then run every test
make check TEST_ARGS="echo"         # only tests whose id contains "echo"
make check TEST_ARGS="-v -j 1"      # one at a time, print every transcript
make check TEST_EXPLICIT="process-server"  # only the explicit tests named process-server
python3 test/run_tests.py --list    # list the test ids
```

*Explicit* tests, such as the tests of a feature that is still being written, never run by
default. A test file makes its tests explicit with `EXPLICIT = "name"` at module level. They
run only when `TEST_EXPLICIT` gives that exact name (a substring does not match, unlike
`TEST_ARGS`), and then only they run. `TEST_ARGS` can still narrow them down.

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
`two-signals`, `pause-race`, `exec`, `exec-loop N`), prints `ok` or `FAIL` for
each check, and exits with status 0 only if all passed. `test_probe.py` runs
every case.

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
                          the queued-pause race, what execve keeps, 1000 execs in a row
test/test_process.py      signals, process groups/sessions, waitpid semantics
test/test_process_server.py  explicit ("process-server"): process management lives in the
                          process server (pids, exit statuses, zombies), not the kernel
test/test_shell_redirection.py  >, >>, <, 2> and 2>&1, backed by the kernel's files
```

## Writing a test

A test is a function `test_*(sh)` in a `test/test_*.py` file. It receives a
machine waiting at its first prompt: `sh.run("echo hi")` types a command and
returns its output (`"hi\n"`), `sh.run_raw(keys)` also returns the echoed
line, and `sh.send()` / `sh.expect()` give full control.
