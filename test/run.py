#!/usr/bin/env python3
"""Boot HaisuliOS in QEMU, run shell commands over the serial console, and check
their output. Used by `make check`; needs qemu-system-x86_64 on PATH."""
import os, select, subprocess, sys, time

KERNEL = sys.argv[1] if len(sys.argv) > 1 else "build/kernel32.elf"
PROMPT = b"\n$ "

# (command, strings that must all appear in its output)
CASES = [
    ("pipetest", ["Wrote 'This is a test', 14 bytes",
                  "Read 'This is a test', 14 bytes"]),
    ("pipecheck", ["pipecheck: passed (0 failures)"]),
    ("ls /bin | cat", ["\ncat\n", "\npipetest\n", "\nsh"]),
    ("echo one two | cat | cat | cat", ["\none two"]),
    ("cat /etc/motd|cat", ["Welcome to HaisuliOS"]),
    ("ls /etc | nosuch", ["sh: nosuch: not found"]),
    ("ls |", ["sh: syntax error near '|'"]),
    ("cat /etc/nosuch | cat", ["cat: /etc/nosuch: No such file or directory"]),
    ("hello | cat", ["reaped pid", "exit status 12"]),
] + [("pipetest", ["Read 'This is a test', 14 bytes"])] * 40  # > NPIPE: exit must free pipes


def main():
    q = subprocess.Popen(
        ["qemu-system-x86_64", "-kernel", KERNEL, "-m", "128M", "-nographic",
         "-no-reboot"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT)
    buf = b""

    def until(marker, timeout=20):
        nonlocal buf
        end = time.time() + timeout
        while marker not in buf:
            if time.time() > end or q.poll() is not None:
                raise TimeoutError(buf[-400:].decode(errors="replace"))
            if select.select([q.stdout], [], [], 0.5)[0]:
                buf += os.read(q.stdout.fileno(), 4096).replace(b"\r", b"")
        out, buf = buf.split(marker, 1)
        return out.decode(errors="replace")

    fails = 0
    try:
        until(PROMPT, 60)  # boot; typing earlier would be lost in serial_init
        for cmd, want in CASES:
            q.stdin.write(cmd.encode() + b"\n")
            q.stdin.flush()
            out = until(PROMPT)
            missing = [w for w in want if w not in out]
            bad = [w for w in ("KERNEL PANIC", "[exception") if w in out]
            if missing or (bad and "crash" not in cmd):
                fails += 1
                print(f"FAIL: {cmd!r}\n  missing {missing or bad}\n{out}")
            else:
                print(f"ok:   {cmd}")
    except TimeoutError as e:
        fails += 1
        print(f"FAIL: timed out; last output:\n{e}")
    finally:
        q.kill()
    print(f"{len(CASES) - fails}/{len(CASES)} passed" if not fails
          else f"{fails} failure(s)")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
