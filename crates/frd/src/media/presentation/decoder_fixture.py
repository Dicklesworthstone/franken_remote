#!/usr/bin/python3
# Explicit synthetic decoder for real input/receipt transport tests; not HEVC.
import sys, time
def read(n):
    data = bytearray()
    while len(data) < n:
        part = sys.stdin.buffer.read(n - len(data))
        if not part: raise EOFError()
        data += part
    return bytes(data)
try:
    while True:
        h = read(36)
        kind = int.from_bytes(h[6:8], 'big')
        b = read(int.from_bytes(h[32:36], 'big'))
        if kind == 8: k, out = 266, b
        elif kind in (4, 6):
            time.sleep(.09)
            k, out = (261 if kind == 4 else 264), b[:8]
        elif kind == 5: k, out = 262, b''
        else: raise ValueError(kind)
        sys.stdout.buffer.write(h[:6] + k.to_bytes(2, 'big') + h[8:32] + len(out).to_bytes(4, 'big') + out)
        sys.stdout.buffer.flush()
        if kind == 5: break
except (EOFError, BrokenPipeError):
    pass
