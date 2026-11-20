#!/usr/bin/python3
"""Test-only completed-drop IPC; opaque payloads do not qualify native HEVC."""
import os
import struct

SKIPS = @SKIPS@
SAME_ORIGINAL = @SAME_ORIGINAL@

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
bitrate = struct.unpack(">I", config[12:16])[0]
reply(h, 257, config)
last = None
candidate = None
original = None
skipped = 0
while True:
    h = read(36)
    b = read(struct.unpack(">I", h[32:])[0])
    kind = struct.unpack(">H", h[6:8])[0]
    if kind == 5:
        reply(h, 262, b"")
        raise SystemExit(0)
    if kind not in (2, 7):
        raise SystemExit(8)  # A completed drop must never be followed by Poll.
    frame, observed, force = struct.unpack(">QQB", b)
    if candidate is not None and frame != candidate + 1:
        raise SystemExit(9)
    candidate = frame
    if last is not None and (SKIPS < 0 or skipped < SKIPS):
        if SAME_ORIGINAL:
            if original is None:
                original = observed
            elif original != observed:
                raise SystemExit(10)
        skipped += 1
        reply(h, 283, struct.pack(">QBI", frame, 1, bitrate))
        continue
    if SAME_ORIGINAL and original is not None and original != observed:
        raise SystemExit(11)
    idr = last is None or force
    reference = 0 if idr else last
    reply(h, 258, struct.pack(">QQQQB7x", frame, observed, generation, reference,
                             0 if idr else 1) + b"skipped-test-only" * 200)
    last = frame
    original = None
