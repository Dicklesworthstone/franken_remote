#!/usr/bin/python3
"""Private IPC fixture. It never decodes HEVC or produces presentation evidence."""
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

def reply(header, kind, body):
    os.write(1, header[:6] + struct.pack(">H", kind) + header[8:32]
             + struct.pack(">I", len(body)) + body)

header = read(36)
body = read(struct.unpack(">I", header[32:])[0])
assert struct.unpack(">H", header[6:8])[0] == 15
assert struct.unpack(">II", body[:8]) == (320, 240)
assert struct.unpack(">III", body[-12:]) == (19, 160, 160)
if MODE == "stall":
    time.sleep(60)
if MODE == "changed-target":
    body = body[:-12] + struct.pack(">III", 20, 160, 160)
reply(header, 272 if MODE == "native-only" else 273, body)
header = read(36)
assert struct.unpack(">H", header[6:8])[0] == 5
assert struct.unpack(">I", header[32:])[0] == 0
reply(header, 262, b"")
