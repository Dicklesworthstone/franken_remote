#!/usr/bin/python3
# Test-only PROTOCOL fixture for the `fr-input-agent --clipboard` IPC. It owns
# no X11 selection: an in-memory item stands in for CLIPBOARD, and a file
# (LOG + ".copy") stands in for a local application's copy. The log records
# message kinds and lengths only. Real X11 effects are qualified separately by
# fr-native's Xvfb test and the namespace e2e with the real child.
import os, struct, sys, time

MODE = "@MODE@"
LOG = "@LOG@"
COPY = LOG + ".copy"

def log(line):
    with open(LOG, "a") as f:
        f.write(line + "\n")

if sys.argv[1:] != ["--clipboard", "--parent-pid", str(os.getppid())]:
    log("BADARGS")
    sys.exit(3)
log("START %d %s" % (os.getpid(), ",".join(sorted(os.environ))))

def read(n):
    b = b""
    while len(b) < n:
        part = os.read(0, n - len(b))
        if not part:
            raise EOFError()
        b += part
    return b

def reply(seq, kind, body=b"", payload=b""):
    os.write(0, b"FRCB" + bytes([1, kind, 0, 0]) + struct.pack(">Q", seq)
             + body.ljust(48, b"\0") + payload)

state = {"revision": 0, "text": None, "origin": None, "pending": None,
         "prepared": None, "reading": False, "max": 0}

def rev():
    return struct.pack(">Q", state["revision"])

def change(origin):
    state["revision"] += 1
    state["pending"] = (state["revision"], state["text"] is not None, origin)

expected = 1
try:
    while True:
        h = read(64)
        if h[:4] != b"FRCB" or h[4] != 1 or h[6:8] != b"\0\0":
            log("BADFRAME")
            sys.exit(5)
        kind, seq, body = h[5], struct.unpack(">Q", h[8:16])[0], h[16:]
        if seq != expected:
            log("BADSEQ")
            sys.exit(6)
        expected += 1
        if kind == 1:
            state["max"] = struct.unpack(">I", body[16:20])[0]
            log("HELLO %d" % state["max"])
            if MODE == "refuse":
                reply(seq, 0x82, b"\x01")
                sys.exit(0)
            reply(seq, 0x81, body[:16])
        elif kind == 2:
            log("WATCH")
            change(None)
            reply(seq, 0x83, rev())
        elif kind == 3:
            if os.path.exists(COPY):
                with open(COPY, "rb") as f:
                    state["text"] = f.read()
                os.unlink(COPY)
                state["origin"] = None
                change(None)
            p, state["pending"] = state["pending"], None
            flags = 0b100
            extra = b""
            if p:
                flags |= 1 | (0b10 if p[1] else 0) | (0b1000 if p[2] else 0)
                extra = struct.pack(">Q", p[0]) + (p[2] or b"")
            reply(seq, 0x84, rev() + bytes([flags]) + extra)
        elif kind == 4:
            stamp, has_rev = body[:25], body[25]
            want = struct.unpack(">Q", body[26:34])[0]
            n = struct.unpack(">I", body[34:38])[0]
            text = read(n)
            log("PREPARE %d" % n)
            if has_rev and want != state["revision"]:
                reply(seq, 0x86, rev() + b"\x04")
            else:
                state["prepared"] = (stamp, text)
                reply(seq, 0x85, rev())
        elif kind == 5:
            stamp = body[:25]
            not_after = struct.unpack(">Q", body[25:33])[0]
            log("PUBLISH-WINDOW %d" % (not_after - time.clock_gettime_ns(time.CLOCK_MONOTONIC)))
            if MODE == "late":
                time.sleep(0.15)
            p = state["prepared"]
            if p is None or p[0] != stamp:
                reply(seq, 0x87, rev() + b"\x02\x03")
            elif time.clock_gettime_ns(time.CLOCK_MONOTONIC) >= not_after:
                log("PUBLISH-LATE")
                reply(seq, 0x87, rev() + b"\x02\x03")
            else:
                state["text"], state["origin"], state["prepared"] = p[1], stamp, None
                state["reading"] = False
                change(stamp)
                log("PUBLISH %d" % len(p[1]))
                reply(seq, 0x87, rev() + b"\x01")
        elif kind == 6:
            state["prepared"] = None
            reply(seq, 0x88, rev())
        elif kind == 7:
            if state["text"] is None:
                reply(seq, 0x8b, rev() + b"\x08")
            else:
                state["reading"] = True
                reply(seq, 0x88, rev())
        elif kind == 8:
            if MODE == "hang":
                log("HANG")
                time.sleep(30)
            if not state["reading"]:
                reply(seq, 0x8b, rev() + b"\x07")
                continue
            state["reading"] = False
            text = state["text"]
            n = len(text)
            if MODE == "oversize":
                n = state["max"] + 1
                text = b""
            elif MODE == "badutf8":
                text = b"\xff\xfe"
                n = 2
            origin = state["origin"]
            meta = struct.pack(">I", n) + (b"\x01" + origin if origin else b"\x00")
            log("READ %d" % n)
            reply(seq, 0x8a, rev() + meta, text)
        elif kind == 9:
            state["reading"] = False
            reply(seq, 0x88, rev())
        elif kind == 10:
            log("SUSPEND")
            reply(seq, 0x88, rev())
        elif kind == 11:
            log("STOP")
            reply(seq, 0x8c)
            sys.exit(0)
        else:
            log("BADKIND")
            sys.exit(7)
except EOFError:
    log("EOF")
