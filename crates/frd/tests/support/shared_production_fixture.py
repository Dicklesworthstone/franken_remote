#!/usr/bin/python3
"""Synthetic capture/Poll responses for physical-credit tests, not HEVC."""
import os
import struct
import time

MODE = "@MODE@"

def read(n):
    body = b""
    while len(body) < n:
        chunk = os.read(0, n - len(body))
        if not chunk:
            raise SystemExit(0)
        body += chunk
    return body

def reply(h, kind, body):
    packet = h[:6] + struct.pack(">H", kind) + h[8:32] + struct.pack(">I", len(body)) + body
    while packet:
        packet = packet[os.write(1, packet):]

h = read(36)
config = read(struct.unpack(">I", h[32:])[0])
generation = struct.unpack(">Q", config[20:28])[0]
reply(h, 257, config)
last = None
pending = None
polls = 0
while True:
    h = read(36)
    b = read(struct.unpack(">I", h[32:])[0])
    kind = struct.unpack(">H", h[6:8])[0]
    if kind == 5:
        reply(h, 262, b"")
        raise SystemExit(0)
    if kind == 3:
        assert pending is not None
        polls += 1
        if polls < 3:
            reply(h, 259, b"")
            continue
        frame, observed, force = pending
        pending = None
    else:
        assert kind in (2, 7) and len(b) == 17
        frame, observed, force = struct.unpack(">QQB", b)
        if MODE == "poll":
            pending = (frame, observed, force)
            polls = 0
            reply(h, 259, b"")
            continue
    if MODE == "delay":
        time.sleep(0.05)
    idr = last is None or force
    reference = 0 if idr else last
    reply(h, 258, struct.pack(">QQQQB7x", frame, observed, generation, reference, 0 if idr else 1) + b"shared-test-only" * 200)
    last = frame
