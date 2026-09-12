#!/usr/bin/python3
# Explicit test codec: real supervised IPC, synthetic opaque encoded payload.
import struct,sys,time
MODE = '@MODE@'
def read(n):
    data = bytearray()
    while len(data) < n:
        part = sys.stdin.buffer.read(n - len(data))
        if not part: raise EOFError()
        data += part
    return bytes(data)
def receive():
    h = read(36)
    return h, int.from_bytes(h[6:8], 'big'), read(int.from_bytes(h[32:36], 'big'))
def reply(h, kind, body=b''):
    sys.stdout.buffer.write(h[:6] + kind.to_bytes(2,'big') + h[8:32] + len(body).to_bytes(4,'big') + body)
    sys.stdout.buffer.flush()
try:
    header,kind,configuration = receive()
    assert kind == 1
    reply(header,257,configuration)
    last = None
    start = time.monotonic()
    while True:
        h,k,b = receive()
        if k == 5: reply(h,262); break
        assert k in (2,7)
        frame,observed,forced = struct.unpack('>QQB',b)
        if last is not None and MODE == 'stall': time.sleep(60)
        if last is not None and MODE == 'slow': time.sleep(.06)
        if last is not None and MODE == 'overloaded': time.sleep(.18)
        if last is not None and (MODE == 'unchanged' or (MODE == 'wake' and time.monotonic() - start < 1.65)) and not forced:
            reply(h,265,struct.pack('>QQQ',frame,last,observed)); continue
        reference = 0 if last is None or forced else last
        predicted = int(last is not None and not forced)
        payload = struct.pack('>QQQQB',frame,observed,0,reference,predicted) + bytes(7) + bytes([frame%255])*128
        reply(h,258,payload)
        last = frame
except (EOFError,BrokenPipeError):
    pass
