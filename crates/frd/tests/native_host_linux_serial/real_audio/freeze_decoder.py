"""Test-only decoder fault: pin one original client's child, resume on EOF.

No numeric-PID signal fallback is permitted. The Rust caller retains stdin
through the fault interval and closes it on all returns, including unwinding.
"""
import os
import pathlib
import select
import signal
import sys
import time


def identity(pid):
    """Return parent, start ticks and observed state without trusting comm."""
    text = pathlib.Path(f"/proc/{pid}/stat").read_text()
    fields = text.rsplit(")", 1)[1].split()
    return int(fields[1]), int(fields[19]), fields[0]


def decoder_identity(pid, owner):
    parent, start, _ = identity(pid)
    image = pathlib.Path(os.readlink(f"/proc/{pid}/exe")).name
    return start if parent == owner and image == "fr-opus-worker" else None


def exited(fd):
    poll = select.poll()
    poll.register(fd, select.POLLIN)
    return bool(poll.poll(0))


def find_decoder(owner):
    until = time.monotonic() + 5
    while time.monotonic() < until:
        for entry in pathlib.Path("/proc").iterdir():
            if time.monotonic() >= until:
                break
            if not entry.name.isdigit():
                continue
            fd = None
            try:
                pid = int(entry.name)
                start = decoder_identity(pid, owner)
                if start is None:
                    continue
                fd = os.pidfd_open(pid)
                # Recheck after pinning. If the PID was recycled during these
                # reads, an exited pidfd cannot signal the replacement process.
                if decoder_identity(pid, owner) == start and not exited(fd):
                    pinned = fd
                    fd = None
                    return pinned, pid, start
            except (OSError, ValueError, IndexError):
                pass
            finally:
                if fd is not None:
                    os.close(fd)
        time.sleep(0.01)
    raise RuntimeError("no original client decoder to freeze")


def freeze(owner):
    fd, pid, start = find_decoder(owner)
    try:
        signal.pidfd_send_signal(fd, signal.SIGSTOP)
        until = time.monotonic() + 0.25
        while True:
            parent, current_start, state = identity(pid)
            if parent != owner or current_start != start or exited(fd):
                raise RuntimeError("decoder ended before its stop was observed")
            if state == "T":
                break
            if time.monotonic() >= until:
                raise RuntimeError("decoder stop was not independently observed")
            time.sleep(0.001)
        # This is observed process state, not just successful signal submission.
        print("STOPPED", flush=True)
        sys.stdin.buffer.read(1)
    finally:
        try:
            signal.pidfd_send_signal(fd, signal.SIGCONT)
        except ProcessLookupError:
            # Its original supervisor may legitimately kill the stalled child.
            pass
        finally:
            os.close(fd)


if __name__ == "__main__":
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        raise SystemExit("decoder fault test requires Python pidfd support")
    try:
        freeze(int(sys.argv[1]))
    except (OSError, ValueError, IndexError, RuntimeError):
        raise SystemExit("original decoder fault could not be established") from None
