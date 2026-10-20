#!/usr/bin/python3
"""Supervised IPC fixture; opaque payloads do not qualify HEVC or native capture."""
import os
import struct
import time


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
last = None
while True:
    header = read(36)
    body = read(struct.unpack('>I', header[32:])[0])
    kind = struct.unpack('>H', header[6:8])[0]
    if kind == 5:
        reply(header, 262, b'')
        raise SystemExit(0)
    assert kind in (2, 7) and len(body) == 17
    frame, observed, force = struct.unpack('>QQB', body)
    # Bootstrap must be immediately useful. Only subsequent native work crosses
    # the failed viewer's 100 ms recovery bound, below the healthy 250 ms bound.
    if last is not None:
        time.sleep(0.16)
    idr = last is None or force
    reference = 0 if idr else last
    reply(header, 258,
          struct.pack('>QQQQB7x', frame, observed, generation, reference,
                      0 if idr else 1)
          + b'shared-recovery-fixture' * 24)
    last = frame
