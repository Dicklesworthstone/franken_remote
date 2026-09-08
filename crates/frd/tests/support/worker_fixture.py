#!/usr/bin/python3
"""Test-only hostile/stalled child; never a production media backend."""
import os, struct, sys, time
MODE = "@MODE@"
def read(n):
    data = b""
    while len(data) < n:
        chunk = os.read(0, n-len(data))
        if not chunk:
            raise SystemExit(0)
        data += chunk
    return data
def reply(header, kind, body):
    os.write(1, header[:6]+struct.pack('>H', kind)+header[8:32]+struct.pack('>I', len(body))+body)
h = read(36)
b = read(struct.unpack('>I', h[32:])[0])
reply(h, 257, b)
while True:
    h = read(36)
    read(struct.unpack('>I', h[32:])[0])
    kind = struct.unpack('>H', h[6:8])[0]
    if kind == 5:
        reply(h, 262, b'')
        raise SystemExit(0)
    if MODE == 'stall':
        time.sleep(60)
    elif MODE == 'partial':
        os.write(1, h[:4])
        time.sleep(60)
    elif MODE == 'wrong-sequence':
        forged = h[:24]+struct.pack('>Q', 999)+h[32:]
        reply(forged, 259, b'')
    elif MODE == 'oversize':
        os.write(1, h[:6]+struct.pack('>H',258)+h[8:32]+struct.pack('>I',0xffffffff))
        time.sleep(60)
    elif MODE == 'eof':
        raise SystemExit(3)
    else:
        reply(h, 259, b'')
