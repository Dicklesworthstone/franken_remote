#!/usr/bin/python3
# Native integration fault injection, not a codec implementation or production path.
import os, subprocess, sys, time
args = sys.argv[1:]
args[args.index("--parent-pid") + 1] = str(os.getpid())
child = subprocess.Popen([@IMAGE@] + args, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def read(stream, n):
    data = bytearray()
    while len(data) < n:
        part = stream.read(n - len(data))
        if not part: raise EOFError()
        data += part
    return bytes(data)
try:
    pictures = 0
    while True:
        header = read(sys.stdin.buffer, 36)
        kind = int.from_bytes(header[6:8], 'big')
        body = read(sys.stdin.buffer, int.from_bytes(header[32:36], 'big'))
        if kind in (4, 6):
            pictures += 1
            if pictures > 1: time.sleep(@DELAY@ / 1000)
        child.stdin.write(header + body)
        child.stdin.flush()
        reply = read(child.stdout, 36)
        result = read(child.stdout, int.from_bytes(reply[32:36], 'big'))
        sys.stdout.buffer.write(reply + result)
        sys.stdout.buffer.flush()
        if kind == 5: break
except (EOFError, BrokenPipeError):
    pass
finally:
    child.kill()
    child.wait()
