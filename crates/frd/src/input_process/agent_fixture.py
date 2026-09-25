#!/usr/bin/python3
# Test-only PROTOCOL fixture for the fr-input-agent IPC. It performs no native
# input at all: "EFFECT" lines in the log stand in for XTest calls. Real X11
# effects are qualified separately by fr-native's Xvfb test with the real child.
import os, select, socket, struct, sys, time

MODE = "@MODE@"
LOG = "@LOG@"
FENCE = b"FRIF\x01\x01" + bytes(10)
LOCAL_REVOKE = b"FRIF\x01\x02" + bytes(10)

def log(line):
    with open(LOG, "a") as f:
        f.write(line + "\n")

if sys.argv[1:] != ["--parent-pid", str(os.getppid())]:
    log("BADARGS")
    sys.exit(3)
log("START %d %s" % (os.getpid(), ",".join(sorted(os.environ))))
sig = socket.socket(fileno=1)
sig.setblocking(False)
fenced = False

def drain():
    global fenced
    while True:
        try:
            d = sig.recv(64)
        except BlockingIOError:
            return
        if d != FENCE:
            log("BADSIGNAL")
            sys.exit(4)
        if not fenced:
            log("FENCE")
        fenced = True

def read(n):
    b = b""
    while len(b) < n:
        part = os.read(0, n - len(b))
        if not part:
            raise EOFError()
        b += part
    return b

def reply(seq, kind, body=b""):
    os.write(0, b"FRIA" + bytes([1, kind, 0, 0]) + struct.pack(">Q", seq) + body.ljust(48, b"\0"))

NAMES = {1: "KEY", 2: "ABS", 3: "BUTTON", 4: "REL", 5: "SCROLL", 6: "WHEEL", 7: "TEXT"}
expected = 1
prepared = None
try:
    while True:
        h = read(64)
        if h[:4] != b"FRIA" or h[4] != 1 or h[6:8] != b"\0\0":
            log("BADFRAME")
            sys.exit(5)
        kind, seq, body = h[5], struct.unpack(">Q", h[8:16])[0], h[16:]
        if seq != expected:
            log("BADSEQ")
            sys.exit(6)
        expected += 1
        op = body[:16]
        release = (op[0] == 1 and op[3] == 0) or (op[0] in (3, 6) and op[2] == 0)
        name = NAMES.get(op[0], "?") + (" RELEASE" if release else "")
        drain()
        if kind == 1:
            log("HELLO")
            # Echo the launch epoch; offer exactly the required capabilities.
            reply(seq, 0x81, body[:16] + body[32:34] + b"\x03")
        elif kind == 2:
            if fenced and not release:
                log("PREPARE-FENCED " + name)
                reply(seq, 0x89)
            else:
                prepared = op
                log("PREPARE " + name)
                reply(seq, 0x83)
        elif kind == 3:
            not_after = struct.unpack(">Q", body[16:24])[0]
            if prepared != op:
                prepared = None
                reply(seq, 0x86, b"\x01")
                continue
            prepared = None
            if MODE == "hang":
                log("HANG " + name)
                time.sleep(30)
            if MODE == "slow":
                # A descheduled child: its own clock check happens late.
                time.sleep(0.05)
            if MODE == "fence-wait" and not release:
                # A stalled native preflight: only frd's fence ends the wait.
                log("WAITING " + name)
                until = time.monotonic() + 5
                while not fenced and time.monotonic() < until:
                    select.select([sig], [], [], 0.05)
                    drain()
            drain()
            if fenced:
                log("SUBMIT-FENCED " + name)
                reply(seq, 0x89)
                continue
            if time.monotonic_ns() >= not_after:
                log("SUBMIT-EXPIRED " + name)
                reply(seq, 0x88)
                continue
            log("EFFECT " + name)
            reply(seq, 0x85)
            if MODE == "local-revoke" and not release:
                fenced = True
                log("LOCAL-REVOKE")
                sig.send(LOCAL_REVOKE)
            if MODE == "die-after-effect":
                log("DIE")
                os._exit(9)
        elif kind == 4:
            if prepared != op or not release:
                prepared = None
                reply(seq, 0x86, b"\x01")
                continue
            prepared = None
            log("CLEANUP-EFFECT " + name)
            reply(seq, 0x85)
        elif kind == 5:
            prepared = None
            log("CANCEL")
            reply(seq, 0x8A)
        elif kind == 6:
            log("CLEANUP")
            reply(seq, 0x8B, b"\x01")
        elif kind == 7:
            log("STOP")
            reply(seq, 0x8C)
            break
        else:
            log("BADKIND")
            sys.exit(7)
except (EOFError, BrokenPipeError, ConnectionResetError):
    log("EOF")
