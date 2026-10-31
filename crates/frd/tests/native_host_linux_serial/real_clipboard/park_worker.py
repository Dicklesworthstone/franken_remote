"""Stop only the real clipboard worker at its lock-free, timed park entry.

Test-only Linux debugger fixture, never loaded by either shipped binary. The
namespace runner uses mapped root or explicit sudo; attachment also requires
the kernel and host security policy to permit ptrace. A refusal fails visibly;
the fixture neither changes security settings nor silently skips the test.
"""
import ctypes as c
import os
from pathlib import Path
import select
import signal
import sys
import time


class Entry(c.Structure):
    _fields_ = [("nr", c.c_uint64), ("args", c.c_uint64 * 6)]


class Payload(c.Union):
    _fields_ = [("entry", Entry), ("padding", c.c_ubyte * 64)]


class Syscall(c.Structure):
    _fields_ = [("op", c.c_ubyte), ("pad", c.c_ubyte * 3),
                ("arch", c.c_uint32), ("ip", c.c_uint64),
                ("sp", c.c_uint64), ("data", Payload)]


libc = c.CDLL(None, use_errno=True)
libc.ptrace.restype = c.c_long
libc.ptrace.argtypes = [c.c_uint, c.c_int, c.c_void_p, c.c_void_p]
SEIZE, INTERRUPT, INFO, SYSCALL, DETACH = 0x4206, 0x4207, 0x420e, 24, 17
WALL, TRACESYSGOOD = 0x40000000, 1
pid = int(sys.argv[1])
tasks = Path(f"/proc/{pid}/task")
tids = [int(p.name) for p in tasks.iterdir()
        if (p / "comm").read_text().strip() == "fr-clipboard"]
assert len(tids) == 1, f"expected one original clipboard worker, got {tids}"
tid = tids[0]
attached = stopped = False


def trace(request, address=0, data=0):
    value = libc.ptrace(request, tid, address, data)
    if value == -1:
        error = c.get_errno()
        raise OSError(error, os.strerror(error))
    return value


def wait_stop(until):
    while time.monotonic() < until:
        found, status = os.waitpid(tid, WALL | os.WNOHANG)
        if found:
            assert os.WIFSTOPPED(status), f"clipboard worker exited: {status}"
            return status
        time.sleep(0.001)
    raise TimeoutError("clipboard worker did not reach a debugger stop")


try:
    trace(SEIZE, data=TRACESYSGOOD)
    attached = True
    trace(INTERRUPT)
    limit = time.monotonic() + 5
    wait_stop(limit)
    stopped = True
    # Never retain the asynchronous interrupt: it could have caught a shared
    # mutex. Trace forward to the worker loop's std::thread::park_timeout.
    # Linux Rust's parker waits for u32::MAX, with a non-null timeout, on a
    # private FUTEX_WAIT_BITSET. Ordinary mutex waits have a different value;
    # native XCB waits and thread::sleep use different syscalls. Worker::step
    # has RETURNED at this park, so no bridge/control/native lock is retained.
    for _ in range(20_000):
        trace(SYSCALL)
        stopped = False
        status = wait_stop(limit)
        stopped = True
        assert os.WSTOPSIG(status) == signal.SIGTRAP | 0x80, status
        info = Syscall()
        size = trace(INFO, c.sizeof(info), c.addressof(info))
        if info.op != 1:  # Only syscall ENTRY, never after a racing wake.
            continue
        assert size >= 80, size
        futex = {0xc000003e: 202, 0xc00000b7: 98}.get(info.arch)
        assert futex is not None, f"unsupported Linux audit architecture {info.arch:#x}"
        entry = info.data.entry
        # futex consumes these integer arguments as 32-bit values. The syscall
        # registers are 64 bits; libc's variadic syscall entry may leave their
        # upper words unspecified (observed for Rust's MATCH_ANY bitset). Match
        # the kernel's values, retaining the full-width non-null timeout check.
        if (entry.nr == futex and entry.args[1] & 0xffffffff == 137
                and entry.args[2] & 0xffffffff == 0xffffffff and entry.args[3] != 0
                and entry.args[5] & 0xffffffff == 0xffffffff):
            break
    else:
        raise TimeoutError("no lock-free clipboard park entry")
    print(f"PARKED {tid}", flush=True)
    # EOF is the RAII release. A failed parent cannot leave this thread parked
    # indefinitely; both a pipe close and this independent ceiling detach it.
    assert select.select([0], [], [], 30)[0], "clipboard stall exceeded its bound"
    assert os.read(0, 1) == b"", "release by closing the private control pipe"
finally:
    if attached:
        if not stopped:
            trace(INTERRUPT)
            wait_stop(time.monotonic() + 2)
        trace(DETACH)
