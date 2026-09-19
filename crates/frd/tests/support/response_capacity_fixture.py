#!/usr/bin/python3
"""Test-only private IPC; emitted bytes are not HEVC or capture evidence."""
import os
import struct

HEADER_ONLY = @HEADER_ONLY@

def read(n):
    value = b""
    while len(value) < n:
        piece = os.read(0, n - len(value))
        if not piece:
            raise SystemExit(0)
        value += piece
    return value

def write(value):
    while value:
        value = value[os.write(1, value):]

def reply(h, kind, body):
    write(h[:6] + struct.pack(">H", kind) + h[8:32] + struct.pack(">I", len(body)) + body)

h = read(36)
config = read(struct.unpack(">I", h[32:])[0])
generation = struct.unpack(">Q", config[20:28])[0]
reply(h, 257, config)
count = 0
while True:
    h = read(36)
    body = read(struct.unpack(">I", h[32:])[0])
    kind = struct.unpack(">H", h[6:8])[0]
    if kind == 5:
        reply(h, 262, b"")
        raise SystemExit(0)
    assert kind == 2 and len(body) == 17
    count += 1
    if HEADER_ONLY:
        write(h[:6] + struct.pack(">H", 258) + h[8:32] + struct.pack(">I", 1040))
        read(1)  # Never send the payload; the parent must refuse or time out.
        raise SystemExit(8)
    frame, observed, force = struct.unpack(">QQB", body)
    reply(h, 258, struct.pack(">QQQQB7x", frame, observed, generation, 0, 0) + bytes([count]) * 16)
