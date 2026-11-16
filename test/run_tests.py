#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0
"""HaisuliOS test runner.

Runs every test in `test/`, each on its own freshly booted QEMU machine.

Discovers `test_*` functions in `test/test_*.py`. Every test gets its own, freshly booted QEMU
machine (a `haisuli.Machine`, waiting at the first shell prompt) as its only argument. After the
test body, the console transcript is checked for crashes (kernel panics, CPU exceptions,
user-space panics).

Tests run in parallel (one QEMU each). Only the Python standard library is needed. Usually
invoked through `make check`; see --help for options.

A test file can make its tests *explicit* by setting `EXPLICIT = "name"` at module level, e.g.
for the tests of a feature that is not implemented yet. Explicit tests never run by default, and
selectors do not pick them up either: they run only with `--explicit name` (`make check
TEST_EXPLICIT=name`), where `name` must equal the file's `EXPLICIT` exactly. Selectors then
narrow down the explicit tests.

A test runs on a machine with one CPU, unless it asks for more with the `haisuli.cpus(n)`
decorator. `--cpus N` changes the default for the tests that do not ask. A test that asks for N
CPUs may keep them all busy, so it also takes N of the host's CPUs (`--cpu-budget`) while it
runs: emulated CPUs that wait for a host CPU make every timing assumption of a test wrong.
"""
import argparse
import concurrent.futures
import contextlib
import importlib.util
import os
import shutil
import sys
import threading
import time
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.dont_write_bytecode = True  # Keep test/ free of __pycache__.

import haisuli  # noqa: E402


def load_test_modules():
    """[(module name, module)] of every `test_*.py` file, sorted by name."""
    modules = []
    for name in sorted(os.listdir(HERE)):
        if not (name.startswith("test_") and name.endswith(".py")):
            continue
        spec = importlib.util.spec_from_file_location(name[:-3], os.path.join(HERE, name))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        modules.append((name[:-3], module))
    return modules


def explicit_name(module):
    """The name that selects the module's tests if they are explicit, else None."""
    return getattr(module, "EXPLICIT", None)


def discover(modules, selectors, explicit=()):
    """[(test id, function, explicit name or None)] in file and definition order.

    Without `explicit`, only the tests that are not explicit; with it, only the explicit tests
    whose name is one of `explicit` (compared exactly, not as a substring).
    """
    tests = []
    for name, module in modules:
        group = explicit_name(module)
        selected = group in explicit if explicit else group is None
        if not selected:
            continue
        funcs = [
            f for n, f in vars(module).items()
            if n.startswith("test_") and callable(f)
            and getattr(f, "__module__", None) == module.__name__
        ]
        funcs.sort(key=lambda f: f.__code__.co_firstlineno)
        for f in funcs:
            test_id = f"{name}::{f.__name__}"
            if not selectors or any(s in test_id for s in selectors):
                tests.append((test_id, f, group))
    return tests


class Result:
    def __init__(self, test_id, status, seconds, message="", transcript="", stderr=""):
        self.test_id = test_id
        self.status = status  # PASS, FAIL or ERROR
        self.seconds = seconds
        self.message = message
        self.transcript = transcript
        self.stderr = stderr


class CpuBudget:
    """The host CPUs that the machines of the running tests may keep busy."""

    def __init__(self, total):
        self.total = max(1, total)
        self.free = self.total
        self.cond = threading.Condition()

    @contextlib.contextmanager
    def take(self, n):
        """Holds `n` CPUs (at most all of them) while the block runs."""
        n = max(1, min(n, self.total))
        with self.cond:
            while self.free < n:
                self.cond.wait()
            self.free -= n
        try:
            yield
        finally:
            with self.cond:
                self.free += n
                self.cond.notify_all()


def run_one(test_id, func, budget):
    # Only the CPUs that a test asks for itself are expected to be busy.
    with budget.take(getattr(func, "cpus", None) or 1):
        return run_machine(test_id, func)


def run_machine(test_id, func):
    start = time.monotonic()
    machine = None
    try:
        machine = haisuli.Machine(cpus=getattr(func, "cpus", None))
        func(machine)
        machine.check_healthy()
        status, message = "PASS", ""
    except AssertionError:
        status, message = "FAIL", traceback.format_exc()
    except Exception:
        status, message = "ERROR", traceback.format_exc()
    finally:
        if machine is not None:
            machine.close()
    return Result(
        test_id,
        status,
        time.monotonic() - start,
        message,
        machine.transcript if machine else "",
        machine.stderr if machine else "",
    )


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("selectors", nargs="*", help="run only tests whose id contains one of these")
    ap.add_argument("--arch", choices=sorted(haisuli.ARCHES), default="x86_64",
                    help="the architecture of the kernel (default %(default)s)")
    ap.add_argument("--qemu", help="the QEMU binary (default: the architecture's)")
    ap.add_argument("--kernel", default=haisuli.Config.kernel,
                    help="kernel image (x86_64: the 32-bit ELF; aarch64: the Image)")
    ap.add_argument("--modules", default=haisuli.Config.modules,
                    help="comma-separated boot modules")
    ap.add_argument("--timeout", type=float, default=haisuli.Config.timeout,
                    help="seconds to wait for expected output (default %(default)s)")
    ap.add_argument("--cpus", type=int, metavar="N",
                    help="CPUs of each machine whose test does not ask for a number (default 1)")
    ap.add_argument("--cpu-budget", type=int, default=os.cpu_count() or 1, metavar="N",
                    help="host CPUs that the tests' machines may keep busy at once "
                         "(default %(default)s)")
    ap.add_argument("-j", "--jobs", type=int, default=min(os.cpu_count() or 1, 8),
                    help="machines to run in parallel (default %(default)s)")
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="show transcripts of passing tests too")
    ap.add_argument("--explicit", action="append", default=[], metavar="NAME",
                    help="run only the explicit tests named NAME exactly (see above); repeatable")
    ap.add_argument("--list", action="store_true", help="list the tests and exit")
    args = ap.parse_args()

    modules = load_test_modules()
    explicit_names = sorted({explicit_name(m) for _, m in modules} - {None})
    tests = discover(modules, args.selectors, args.explicit)
    if args.list:
        for test_id, _, _ in tests:
            print(test_id)
        if not args.explicit:
            # Explicit tests are not run by default, but they should still be easy to find.
            for test_id, _, group in discover(modules, args.selectors, explicit_names):
                print(f"{test_id} (explicit: {group})")
        return 0
    if not tests:
        if args.explicit:
            known = ", ".join(explicit_names) or "none"
            print(f"no tests selected (explicit test names: {known})")
        else:
            print("no tests selected")
        return 1

    missing = [p for p in [args.kernel, *args.modules.split(",")] if not os.path.exists(p)]
    if missing:
        print(f"missing build output: {', '.join(missing)} (run `make` first)")
        return 1
    arch = haisuli.ARCHES[args.arch]
    qemu = args.qemu or arch.qemu
    if shutil.which(qemu) is None:
        print(f"QEMU not found: {qemu}")
        return 1
    haisuli.Config.arch = arch
    haisuli.Config.qemu = qemu
    haisuli.Config.kernel = args.kernel
    haisuli.Config.modules = args.modules
    haisuli.Config.timeout = args.timeout
    haisuli.Config.cpus = args.cpus

    print(f"running {len(tests)} tests, {args.jobs} at a time")
    start = time.monotonic()
    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        budget = CpuBudget(args.cpu_budget)
        futures = [pool.submit(run_one, test_id, func, budget) for test_id, func, _ in tests]
        for future in concurrent.futures.as_completed(futures):
            r = future.result()
            results.append(r)
            print(f"{r.status:5} {r.test_id} ({r.seconds:.1f}s)", flush=True)
            if args.verbose and r.status == "PASS":
                print(r.transcript)

    failed = [r for r in results if r.status != "PASS"]
    order = {test_id: i for i, (test_id, _, _) in enumerate(tests)}
    for r in sorted(failed, key=lambda r: order[r.test_id]):
        print(f"\n{'=' * 70}\n{r.status}: {r.test_id}\n{'-' * 70}\n{r.message.rstrip()}")
        print(f"{'-' * 70}\nconsole transcript:\n{r.transcript.rstrip()}")
        if r.stderr.strip():
            print(f"{'-' * 70}\nQEMU stderr:\n{r.stderr.rstrip()}")
    passed = len(results) - len(failed)
    print(f"\n{passed} passed, {len(failed)} failed in {time.monotonic() - start:.1f}s")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
