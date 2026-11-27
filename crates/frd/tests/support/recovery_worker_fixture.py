#!/usr/bin/python3
"""Test-only process protocol. Bytes are NOT HEVC or native capture evidence."""
import os
import struct
import time

MODES = set("@MODE@".split("+"))

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
        only_if_changed = bool(flags & 2)
    elif kind in (2, 7) and len(b) == 17:
        frame, observed, force = struct.unpack(">QQB", b)
        target = bitrate
        only_if_changed = kind == 7
    else:
        raise SystemExit(8)
    if "delay-capture" in MODES:
        time.sleep(0.05)
    if force and last is not None and "poll-forever" in MODES:
        pending = True
        reply(h, 259, b"")
        continue
    if only_if_changed and not force and last is not None and "changing" not in MODES:
        if "delay-unchanged" in MODES:
            time.sleep(0.05)
        if "defer-unchanged" in MODES:
            time.sleep(0.1)
        proof = struct.pack(">QQQ", frame, last, observed)
        if kind == 20:
            # A static capture confirms the current rate, never the deferred target.
            reply(h, 279, proof + struct.pack(">I", bitrate))
        else:
            reply(h, 265, proof)
        continue
    if "delay-first-encoded" in MODES and last is not None and not delayed:
        # A host stalled between capture and send: this picture reaches its
        # viewer after the test's reference horizon. Only once.
        delayed = True
        time.sleep(0.06)
    idr = last is None or target != bitrate or (force and "ignore-force" not in MODES)
    reference = 0 if idr else last
    bitrate = target
    prefix = struct.pack(">QQQQB", frame, observed, generation, reference, 0 if idr else 1)
    reserved = struct.pack(">I3x", bitrate) if kind == 20 else bytes(7)
    reply(h, 278 if kind == 20 else 258, prefix + reserved + b"test-only-unit")
    last = frame
