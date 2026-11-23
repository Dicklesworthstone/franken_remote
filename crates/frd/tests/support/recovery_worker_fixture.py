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
bitrate = struct.unpack(">I", config[12:16])[0]
reply(h, 257, config)
last = None
pending = False
delayed = False
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
    if kind == 20 and len(b) == 21:
        frame, observed, flags, target = struct.unpack(">QQBI", b)
        if flags & ~3:
            raise SystemExit(8)
        force = bool(flags & 1)
    elif kind in (2, 7) and len(b) == 17:
        frame, observed, force = struct.unpack(">QQB", b)
        target = bitrate
    else:
        raise SystemExit(8)
    if force and last is not None and MODE == "poll-forever":
        pending = True
        reply(h, 259, b"")
        continue
    if kind == 7 and not force and last is not None:
        if MODE == "delay-unchanged":
            time.sleep(0.05)
        reply(h, 265, struct.pack(">QQQ", frame, last, observed))
        continue
    if kind == 20 and flags & 2 and not force and last is not None:
        # Static content defers the explicit rate on the original native owner.
        # A later forced recovery picture must still carry that same target.
        reply(h, 279, struct.pack(">QQQI", frame, last, observed, bitrate))
        continue
    if MODE == "delay-first-encoded" and last is not None and not delayed:
        # A host stalled between capture and send: this picture reaches its
        # viewer after the test's reference horizon. Only once.
        delayed = True
        time.sleep(0.06)
    idr = last is None or target != bitrate or (force and MODE != "ignore-force")
    reference = 0 if idr else last
    bitrate = target
    prefix = struct.pack(">QQQQB", frame, observed, generation, reference, 0 if idr else 1)
    reserved = struct.pack(">I3x", bitrate) if kind == 20 else bytes(7)
    reply(h, 278 if kind == 20 else 258, prefix + reserved + b"test-only-unit")
    last = frame
