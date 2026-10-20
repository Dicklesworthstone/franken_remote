#!/usr/bin/python3
"""Supervised IPC fixture; opaque payloads are not HEVC or native capture proof."""
import os
import struct
import time

MODE = '@MODE@'


def read(n):
    data = b''
    while len(data) < n:
        chunk = os.read(0, n - len(data))
        if not chunk:
            raise SystemExit(0)
        data += chunk
    return data


def reply(header, kind, body):
    packet = (header[:6] + struct.pack('>H', kind) + header[8:32]
              + struct.pack('>I', len(body)) + body)
    while packet:
        packet = packet[os.write(1, packet):]


header = read(36)
configuration = read(struct.unpack('>I', header[32:])[0])
assert struct.unpack('>H', header[6:8])[0] == 1
generation = struct.unpack('>Q', configuration[20:28])[0]
reply(header, 257, configuration)
pending = None
while True:
    header = read(36)
    body = read(struct.unpack('>I', header[32:])[0])
    kind = struct.unpack('>H', header[6:8])[0]
    if kind == 5:
        reply(header, 262, b'')
        raise SystemExit(0)
    if kind in (2, 7):
        assert pending is None
        pending = struct.unpack('>QQB', body)
        if MODE == 'silent':
            time.sleep(60)
            raise SystemExit(9)
        if MODE in ('poll', 'endless'):
            reply(header, 259, b'')
            continue
    else:
        assert kind == 3 and pending is not None
    time.sleep(0.12 if MODE == 'endless' else 0.7)
    if MODE == 'endless':
        reply(header, 259, b'')
        continue
    frame, observed, force = pending
    assert force
    reply(header, 258,
          struct.pack('>QQQQB7x', frame, observed, generation, 0, 0)
          + b'capture-authority-fixture' * 16)
    pending = None
