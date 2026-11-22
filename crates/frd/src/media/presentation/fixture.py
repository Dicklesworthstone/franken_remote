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
    first=True
    while True:
        h=read(36); b=read(int.from_bytes(h[32:36],'big')); k=int.from_bytes(h[6:8],'big')
        if k==5: reply(h,262); break
        if k==17:
            if MODE=='cursor-snapshot': assert b[-4:]==bytes([1,2,3,255])
            if MODE=='cursor-stall': time.sleep(60)
            if MODE=='cursor-bad': reply(h,261,(999).to_bytes(8,'big'))
            else: reply(h,275)
            continue
        if k==21:
            assert len(b)==24 and MODE.startswith('snapshot-')
            if MODE=='snapshot-absent':
                reply(h,281,bytes([4]))
                continue
            if MODE=='snapshot-over-ceiling':
                # Advertise too much before sending any pixels: the original
                # worker must reject this header without a large allocation.
                sys.stdout.buffer.write(h[:6]+(280).to_bytes(2,'big')+h[8:32]+(32+32*1024*1024+4).to_bytes(4,'big'))
                sys.stdout.buffer.flush()
                continue
            width,height=(640,480)
            if MODE=='snapshot-wide': width,height=(8193,1)
            stamp=bytearray(b)
            for field,offset in [('frame',0),('configuration',8),('recovery',16)]:
                if MODE=='snapshot-wrong-'+field:
                    stamp[offset:offset+8]=(int.from_bytes(b[offset:offset+8],'big')+1).to_bytes(8,'big')
            pixels=bytes([0x17,0x2a,0x3d,255])*(width*height)
            if MODE=='snapshot-short': pixels=pixels[:-4]
            reply(h,280,stamp+width.to_bytes(4,'big')+height.to_bytes(4,'big')+pixels)
            continue
        assert k in (4,6)
        if MODE=='stall': time.sleep(60)
        if MODE=='slow': time.sleep(.04)
        if MODE=='drain': time.sleep(.09)
        if MODE=='slow-first' and first: time.sleep(.35)
        first=False
        if MODE=='bad-reply': b = (999).to_bytes(8, 'big')
        reply(h,261 if k==4 else 264,b[:8])
except (EOFError,BrokenPipeError):
    pass
