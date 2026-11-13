#!/usr/bin/python3
"""Private IPC fixture: no native decoder or visible presentation evidence."""
import os
import struct
import time

MODE = "@MODE@"


def read(length):
    data = b""
    while len(data) < length:
        chunk = os.read(0, length - len(data))
        if not chunk:
            raise SystemExit(0)
        data += chunk
    return data


def reply(header, kind, body):
    os.write(1, header[:6] + struct.pack(">H", kind) + header[8:32]
             + struct.pack(">I", len(body)) + body)


header = read(36)
body = read(struct.unpack(">I", header[32:])[0])
assert struct.unpack(">H", header[6:8])[0] == 22
assert struct.unpack(">II", body[:8]) == (320, 240)
assert struct.unpack(">IIIII", body[-20:]) == (19, 360, 270, 640, 480)
if MODE == "stall":
    time.sleep(60)
if MODE == "changed-target":
    body = body[:-20] + struct.pack(">IIIII", 20, 360, 270, 640, 480)
if MODE == "changed-display":
    body = body[:-8] + struct.pack(">II", 1280, 960)
if MODE == "wrong-epoch":
    header = header[:8] + (72).to_bytes(16, "big") + header[24:]
if MODE == "wrong-sequence":
    header = header[:24] + (1).to_bytes(8, "big") + header[32:]
if MODE == "refused":
    reply(header, 263, struct.pack(">H", 7))
else:
    reply(header, {"native-only": 272, "fitted-only": 273}.get(MODE, 282), body)
header = read(36)
assert struct.unpack(">H", header[6:8])[0] == 5
assert struct.unpack(">I", header[32:])[0] == 0
reply(header, 262, b"")
