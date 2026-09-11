#!/usr/bin/python3
# Test-only decoder IPC peer. Payloads and completion are synthetic, not HEVC.
import struct,sys,time
MODE = '@MODE@'
def read(n):
    b=bytearray()
    while len(b)<n:
        p=sys.stdin.buffer.read(n-len(b))
        if not p: raise EOFError()
        b+=p
    return bytes(b)
def reply(h,k,b=b''):
    sys.stdout.buffer.write(h[:6]+k.to_bytes(2,'big')+h[8:32]+len(b).to_bytes(4,'big')+b)
    sys.stdout.buffer.flush()
try:
    h=read(36); b=read(int.from_bytes(h[32:36],'big')); k=int.from_bytes(h[6:8],'big')
    assert k in (1,8)
    reply(h,257 if k==1 else 266,b)
    while True:
        h=read(36); b=read(int.from_bytes(h[32:36],'big')); k=int.from_bytes(h[6:8],'big')
        if k==5: reply(h,262); break
        assert k in (4,6)
        if MODE=='stall': time.sleep(60)
        if MODE=='slow': time.sleep(.04)
        reply(h,261 if k==4 else 264,b[:8])
except (EOFError,BrokenPipeError):
    pass
