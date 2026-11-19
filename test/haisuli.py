# SPDX-License-Identifier: MIT
"""HaisuliOS test harness.

Boots HaisuliOS in QEMU and drives its serial console like a user at a terminal.

The serial console is connected to pipes, so tests can type at the shell (Enter sends CR, as a
real terminal does) and wait for what appears on the screen. Only the Python standard library is
used.
"""
import codecs
import os
import re
import subprocess
import threading
import time

PROMPT = "haisuli# "

# Text that must never appear on the console in a healthy run.
FAILURE_MARKERS = (
    "KERNEL PANIC",  # The kernel panic handler.
    "[KERNEL]",  # A CPU exception in kernel mode.
    "[user] #",  # A CPU exception in a user process.
    "deadlock",  # The scheduler found nothing runnable.
    "panic:",  # The user-space panic handler.
)


ROOTFS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "rootfs")


def read_rootfs(path):
    """Contents of a file that the build packs into the boot ramdisk."""
    with open(os.path.join(ROOTFS, path), encoding="utf-8") as f:
        return f.read()


class Timeout(AssertionError):
    pass


class Arch:
    """What differs between the architectures: how QEMU runs the machine, and what the tests may
    expect of it."""

    def __init__(self, name, qemu, machine, power_off_status, console, console_info):
        self.name = name  # As the kernel's banner says it.
        self.qemu = qemu  # The default QEMU binary.
        self.machine = machine  # QEMU's arguments for the machine.
        # QEMU's exit status when the kernel powers the machine off.
        self.power_off_status = power_off_status
        # The console device's name and IRQ line, as the console server logs them.
        self.console = console
        # What `probe_test devices` prints about it.
        self.console_info = console_info


ARCHES = {
    "x86_64": Arch(
        "x86_64", "qemu-system-x86_64",
        # The kernel powers off through isa-debug-exit, which makes QEMU exit with
        # (code << 1) | 1: status 1 for code 0.
        ["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"], 1,
        ("COM1", 4), "console: COM1 kind 1 space 1 base 0x3f8 len 8 irq 4",
    ),
    "aarch64": Arch(
        "aarch64", "qemu-system-aarch64",
        # PSCI SYSTEM_OFF makes QEMU exit with status 0.
        ["-M", "virt,gic-version=3", "-cpu", "cortex-a72"], 0,
        ("UART0", 1), "console: UART0 kind 2 space 2 base 0x9000000 len 4096 irq 1",
    ),
}


class Config:
    """How to start QEMU. Filled in by the runner from its command line."""

    arch = ARCHES["x86_64"]
    qemu = "qemu-system-x86_64"
    kernel = "build/haisulios.elf32"
    modules = "build/init,build/proc,build/initrd.tar"
    timeout = 15.0  # Seconds to wait for any expected output.
    cpus = None  # CPUs of a machine whose test does not ask for a number (None: QEMU's default, 1).


def cpus(n):
    """Decorator: the test runs on a machine with `n` CPUs (QEMU's `-smp n`)."""
    def mark(test):
        test.cpus = n
        return test
    return mark


class Machine:
    """One QEMU instance running HaisuliOS, booted to the first prompt.

    It has `cpus` CPUs, or `config.cpus` if that is None.
    """

    def __init__(self, config=Config, cpus=None):
        self.config = config
        self.cpus = cpus if cpus is not None else config.cpus
        # Everything the console printed so far, decoded, with the terminal's
        # CR LF line ends turned into LF.
        self._text = ""
        self._decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self._held_cr = ""  # A trailing CR whose LF may still be on its way.
        self._mark = 0  # The start of the output not yet consumed by a test.
        self._allowed = set()  # Failure markers that the test expects (see `allow`).
        self._cond = threading.Condition()
        self._eof = False
        cmd = [
            config.qemu,
            "-kernel", config.kernel,
            "-initrd", config.modules,
            "-m", "128M",
            "-serial", "stdio",
            "-monitor", "none",
            "-display", "none",
            "-nic", "none",
            "-no-reboot",
            *config.arch.machine,
        ]
        if self.cpus is not None:
            cmd += ["-smp", str(self.cpus)]
        self.proc = subprocess.Popen(
            cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        self._reader = threading.Thread(target=self._read_stdout, daemon=True)
        self._reader.start()
        self._stderr = b""
        self._err_reader = threading.Thread(target=self._read_stderr, daemon=True)
        self._err_reader.start()
        # Everything up to and including the first prompt. If it never comes, nobody gets this
        # machine to close: stop QEMU here.
        try:
            self.boot_output = self.expect(PROMPT)
        except BaseException:
            self.close()
            raise

    # ---- plumbing ---------------------------------------------------------

    def _read_stdout(self):
        fd = self.proc.stdout.fileno()
        while True:
            data = os.read(fd, 4096)
            with self._cond:
                if not data:
                    self._text += self._held_cr + self._decoder.decode(b"", final=True)
                    self._held_cr = ""
                    self._eof = True
                    self._cond.notify_all()
                    return
                chunk = self._held_cr + self._decoder.decode(data)
                self._held_cr = ""
                if chunk.endswith("\r"):
                    chunk, self._held_cr = chunk[:-1], "\r"
                self._text += chunk.replace("\r\n", "\n")
                self._cond.notify_all()

    def _read_stderr(self):
        self._stderr = self.proc.stderr.read()

    @property
    def transcript(self):
        """Everything the console printed so far (CR LF normalised to LF)."""
        with self._cond:
            return self._text

    @property
    def stderr(self):
        return self._stderr.decode("utf-8", "replace")

    # ---- interaction ------------------------------------------------------

    def send(self, keys):
        """Type `keys` (str or bytes) without pressing Enter."""
        if isinstance(keys, str):
            keys = keys.encode("utf-8")
        self.proc.stdin.write(keys)
        self.proc.stdin.flush()

    def expect(self, pattern, timeout=None):
        """Wait until the unconsumed output contains `pattern` (a string, or a
        compiled regular expression). Consumes and returns the output up to
        and including the match."""
        timeout = self.config.timeout if timeout is None else timeout
        deadline = time.monotonic() + timeout
        with self._cond:
            while True:
                text = self._text[self._mark:]
                if isinstance(pattern, re.Pattern):
                    m = pattern.search(text)
                    end = m.end() if m else -1
                else:
                    i = text.find(pattern)
                    end = i + len(pattern) if i >= 0 else -1
                if end >= 0:
                    self._mark += end
                    return text[:end]
                left = deadline - time.monotonic()
                if left <= 0 or self._eof:
                    why = "console closed" if self._eof else f"timed out after {timeout}s"
                    raise Timeout(f"{why} waiting for {pattern!r}; unconsumed output: {text!r}")
                self._cond.wait(left)

    def run(self, line):
        """Type a command line and press Enter; wait for the next prompt.
        Returns the command's output (without the echoed input line and
        without the new prompt)."""
        echo, output = self.run_raw(line + "\r")
        return output

    def run_raw(self, keys):
        """Type `keys` (which must end the line, e.g. with CR) and wait for the
        next prompt. Returns (echo, output): the echoed input line, as drawn
        on the terminal, and the command's output."""
        self.send(keys)
        chunk = self.expect(PROMPT)[: -len(PROMPT)]
        echo, _, output = chunk.partition("\n")
        return echo, output

    def wait_exit(self, timeout=None):
        """Wait for QEMU to terminate; returns its exit status.

        Afterwards the transcript holds everything the console printed."""
        timeout = self.config.timeout if timeout is None else timeout
        try:
            code = self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            raise Timeout(f"QEMU still running after {timeout}s")
        self._reader.join(timeout)
        return code

    def close(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        self._reader.join(2)
        self._err_reader.join(2)
        for f in (self.proc.stdin, self.proc.stdout, self.proc.stderr):
            try:
                f.close()
            except OSError:
                pass

    def allow(self, marker):
        """Let the console show `marker`, one of the FAILURE_MARKERS, because the test makes
        that happen on purpose (e.g. a process that faults)."""
        assert marker in FAILURE_MARKERS, marker
        self._allowed.add(marker)

    def check_healthy(self):
        """Fail if the kernel or a process reported a crash (other than those allowed)."""
        text = self.transcript
        for marker in FAILURE_MARKERS:
            if marker not in self._allowed:
                assert marker not in text, f"console shows {marker!r}"
