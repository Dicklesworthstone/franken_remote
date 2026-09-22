#!/usr/bin/python3
"""Synthetic monitor IPC peer. No X11, pixels, HEVC or permission evidence."""
import os
import struct

MODE = "@MODE@"

def read(n):
    result = b""
    while len(result) < n:
        data = os.read(0, n - len(result))
        if not data:
            raise SystemExit(0)
        result += data
    return result

def record():
    h = read(36)
    return h, struct.unpack(">H", h[6:8])[0], read(struct.unpack(">I", h[32:])[0])

def reply(h, kind, body):
    data = h[:6] + struct.pack(">H", kind) + h[8:32] + struct.pack(">I", len(body)) + body
    while data:
        data = data[os.write(1, data):]

def display(handle, x):
    return handle.to_bytes(16, "big") + struct.pack(">QiiIIIIIIB", 4, x, 40, 320, 240, 320, 240, 1, 1, 0)

h, kind, body = record()
assert kind == 11 and not body
reply(h, 269, struct.pack(">QB", 70, 2) + display(201, -320) + display(202, 0))
configured = False
last = None
while True:
    h, kind, body = record()
    if kind == 5:
        reply(h, 262, b"")
        raise SystemExit(0)
    if kind == 13:
        if MODE == "retired":
            reply(h, 263, struct.pack(">H", 9))
        else:
            reply(h, 271, b"")
        continue
    if kind == 12:
        assert not configured and len(body) == 52
        # Parent must translate its opaque alias to this original native choice.
        assert int.from_bytes(body[28:36], "big") == 70
        assert int.from_bytes(body[36:], "big") in (201, 202)
        if MODE == "refuse-configure":
            reply(h, 263, struct.pack(">H", 9))
            continue
        generation = int.from_bytes(body[20:28], "big")
        configured = True
        reply(h, 270, body)
        continue
    assert configured and kind in (2, 7) and len(body) == 17
    frame, observed, force = struct.unpack(">QQB", body)
    if last is not None and kind == 7 and not force and MODE != "changing":
        reply(h, 265, struct.pack(">QQQ", frame, last, observed))
        continue
    reply(h, 258, struct.pack(">QQQQB7x", frame, observed, generation, 0, 0) + b"synthetic-monitor-unit")
    last = frame
