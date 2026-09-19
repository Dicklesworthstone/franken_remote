#!/usr/bin/python3
# Test-only process; canned HEVC parameter sets and synthetic decode are NOT
# a native capture/codec qualification. Exercise actual production IPC owners.
import struct,sys,time
MODE='@MODE@'
PARAMETERS=b''.join(struct.pack('>I',len(n))+n for n in map(bytes.fromhex,[
    '40010c01ffff01600000030090000003000003003cba0240',
    '42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04',
    '4401c0718112','2801ade06702f86753c11ead2f1f6a69']))
def read(n):
    b=b''
    while len(b)<n:
        p=sys.stdin.buffer.read(n-len(b))
        if not p: raise EOFError()
        b+=p
    return b

def reply(h,k,b=b''):
    sys.stdout.buffer.write(h[:6]+k.to_bytes(2,'big')+h[8:32]+len(b).to_bytes(4,'big')+b)
    sys.stdout.buffer.flush()
try:
    h=read(36); cfg=read(int.from_bytes(h[32:36],'big'))
    reply(h,257,cfg)
    generation=int.from_bytes(cfg[20:28],'big')
    last=None
    while True:
        h=read(36); b=read(int.from_bytes(h[32:36],'big')); k=int.from_bytes(h[6:8],'big')
        if k==5: reply(h,262); break
        assert k in (2,7)
        frame,observed,forced=struct.unpack('>QQB',b)
        if frame==2 and not forced:
            time.sleep(60 if MODE=='stall' else .35)
        is_idr=last is None or (forced and MODE!='ignore-force')
        reference=0 if is_idr else last
        payload=PARAMETERS if is_idr else b'synthetic-dependent'
        reply(h,258,struct.pack('>QQQQB7x',frame,observed,generation,reference,int(not is_idr))+payload)
        last=frame
except (EOFError,BrokenPipeError):
    pass
