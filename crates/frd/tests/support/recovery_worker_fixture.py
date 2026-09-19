#!/usr/bin/python3
"""Test-only process protocol. Bytes are NOT HEVC or native capture evidence."""
import os
import struct
import time

MODE = "@MODE@"

def read(n):
    data = b""
    while len(data) < n:
        chunk = os.read(0, n - len(data))
        if not chunk:
            raise SystemExit(0)
        data += chunk
    return data

def reply(h, kind, body):
    packet = h[:6] + struct.pack(">H", kind) + h[8:32] + struct.pack(">I", len(body)) + body
    while packet:
        packet = packet[os.write(1, packet):]

h = read(36)
config = read(struct.unpack(">I", h[32:])[0])
generation = struct.unpack(">Q", config[20:28])[0]
reply(h, 257, config)
last = None
pending = False
while True:
    h = read(36)
    b = read(struct.unpack(">I", h[32:])[0])
    kind = struct.unpack(">H", h[6:8])[0]
    if kind == 5:
        reply(h, 262, b"")
        raise SystemExit(0)
    if kind == 3 and pending:
        reply(h, 259, b"")
        continue
    if kind not in (2, 7) or len(b) != 17:
        raise SystemExit(8)
    frame, observed, force = struct.unpack(">QQB", b)
    if force and last is not None and MODE == "poll-forever":
        pending = True
        reply(h, 259, b"")
        continue
    if kind == 7 and not force and last is not None:
        reply(h, 265, struct.pack(">QQQ", frame, last, observed))
        continue
    idr = last is None or (force and MODE != "ignore-force")
    reference = 0 if idr else last
    reply(h, 258, struct.pack(">QQQQB7x", frame, observed, generation, reference, 0 if idr else 1) + b"test-only-unit")
    last = frame
