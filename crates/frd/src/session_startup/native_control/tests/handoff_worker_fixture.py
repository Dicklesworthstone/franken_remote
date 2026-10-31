#!/usr/bin/python3
# Explicit protocol fixture. No physical monitor, HEVC decoder or OS effects.
import struct, sys, time
CATALOG = bytes.fromhex('@CATALOG@')
MODE = '@MODE@'
NALS = ['40010c01ffff01600000030090000003000003003cba0240',
        '42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04',
        '4401c0718112', '2801ade06702f86753c11ead2f1f6a69']
UNIT = b''.join(len(bytes.fromhex(n)).to_bytes(4, 'big') + bytes.fromhex(n) for n in NALS)
def read(n):
    b = bytearray()
    while len(b) < n:
        part = sys.stdin.buffer.read(n - len(b))
        if not part: raise EOFError()
        b += part
    return bytes(b)
def reply(h, k, b=b''):
    sys.stdout.buffer.write(h[:6] + k.to_bytes(2, 'big') + h[8:32] + len(b).to_bytes(4, 'big') + b)
    sys.stdout.buffer.flush()
last = None
actual_rate = None
try:
    while True:
        h = read(36)
        k = int.from_bytes(h[6:8], 'big')
        b = read(int.from_bytes(h[32:36], 'big'))
        if k == 11: reply(h, 269, CATALOG)
        elif k == 12:
            assert len(b) == 52
            actual_rate = int.from_bytes(b[12:16], 'big')
            reply(h, 270, b)
        elif k == 13: reply(h, 271)
        elif k == 8: reply(h, 266, b)
        elif k in (4, 6): reply(h, 261 if k == 4 else 264, b[:8])
        elif k == 5: reply(h, 262); break
        # ReadCursor: no separate cursor exists here (typed Unsupported).
        elif k == 16: reply(h, 274, b'\x02')
        elif k == 20:
            assert len(b) == 21
            frame, observed, flags, target_rate = struct.unpack('>QQBI', b)
            assert flags & ~3 == 0 and 10_000 <= target_rate <= 200_000_000
            assert actual_rate is not None and 10_000 <= actual_rate <= 200_000_000
            if last is not None and MODE == 'stall': time.sleep(60)
            if last is not None and flags & 2 and not flags & 1:
                # Unchanged work does not claim to have applied the new target.
                reply(h, 279, struct.pack('>QQQI', frame, last, observed, actual_rate))
            else:
                actual_rate = target_rate
                reply(h, 278, struct.pack('>QQQQBI3x', frame, observed, 0, 0, 0, actual_rate) + UNIT)
                last = frame
        elif k in (2, 7):
            frame, observed, forced = struct.unpack('>QQB', b)
            if last is not None and MODE == 'stall': time.sleep(60)
            if last is not None and not forced:
                reply(h, 265, struct.pack('>QQQ', frame, last, observed))
            else:
                reply(h, 258, struct.pack('>QQQQB', frame, observed, 0, 0, 0) + bytes(7) + UNIT)
                last = frame
        else: raise AssertionError(k)
except (EOFError, BrokenPipeError):
    pass
