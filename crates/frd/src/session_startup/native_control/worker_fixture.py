#!/usr/bin/python3
# Test-only capture/decoder IPC. Recorded x265 Main8 IDR is real; source checks,
# discovery, decoder replies and compositor submission are explicit fixtures.
import struct,sys
IDR = bytes.fromhex("0000001840010c01ffff01600000030090000003000003003cba02400000002b42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04000000064401c0718112000000292801ade0d117ffd3917322eebaeda718a00000030000030000030000030000030002bc000003007840")
def read(n):
    b = bytearray()
    while len(b) < n:
        part = sys.stdin.buffer.read(n - len(b))
        if not part: raise EOFError()
        b += part
    return bytes(b)
def reply(h, k, b=b''):
    sys.stdout.buffer.write(h[:6]+k.to_bytes(2,'big')+h[8:32]+len(b).to_bytes(4,'big')+b)
    sys.stdout.buffer.flush()
try:
    last = None
    while True:
        h = read(36)
        k = int.from_bytes(h[6:8],'big')
        b = read(int.from_bytes(h[32:36],'big'))
        if k == 11:
            d = (9).to_bytes(16,'big')+struct.pack('>QiiIIIIIIB',0,-320,0,320,240,320,240,1,1,0)
            reply(h,269,struct.pack('>QB',1,1)+d)
        elif k == 12:
            assert len(b) == 52
            reply(h,270,b)
        elif k == 13: reply(h,271)
        elif k == 8: reply(h,266,b)
        elif k in (4,6): reply(h,261 if k==4 else 264,b[:8])
        elif k in (2,7):
            frame,at,forced = struct.unpack('>QQB',b)
            if last is not None and not forced:
                reply(h,265,struct.pack('>QQQ',frame,last,at))
            else:
                reply(h,258,struct.pack('>QQQQB',frame,at,0,0,0)+bytes(7)+IDR)
                last = frame
        elif k == 5: reply(h,262); break
        # ReadCursor: no separate cursor exists here (typed Unsupported).
        elif k == 16: reply(h,274,b'\x02')
        else: raise ValueError(k)
except (EOFError,BrokenPipeError):
    pass
