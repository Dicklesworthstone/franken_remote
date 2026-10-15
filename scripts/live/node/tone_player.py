# Tone player on a private PulseAudio sink: HZ1 until wall-clock SWITCH, then HZ2.
# Usage: player.py SERVER SINK HZ1 HZ2 SWITCH_EPOCH STOP_EPOCH
import ctypes as c, math, sys, time
class Spec(c.Structure):
    _fields_ = [("format", c.c_int), ("rate", c.c_uint32), ("channels", c.c_uint8)]
class Attr(c.Structure):
    _fields_ = [(n, c.c_uint32) for n in ("maxlength", "tlength", "prebuf", "minreq", "fragsize")]
lib = c.CDLL("libpulse-simple.so.0")
lib.pa_simple_new.argtypes = [c.c_char_p, c.c_char_p, c.c_int, c.c_char_p, c.c_char_p,
                              c.POINTER(Spec), c.c_void_p, c.POINTER(Attr), c.POINTER(c.c_int)]
lib.pa_simple_new.restype = c.c_void_p
lib.pa_simple_write.argtypes = [c.c_void_p, c.c_void_p, c.c_size_t, c.POINTER(c.c_int)]
error = c.c_int()
s = lib.pa_simple_new(sys.argv[1].encode(), b"fr-live-player", 1, sys.argv[2].encode(),
                      b"live-tone", c.byref(Spec(3, 48000, 2)), None,
                      c.byref(Attr(0xffffffff, 3840, 0xffffffff, 0xffffffff, 0xffffffff)), c.byref(error))
assert s, "player connection failed"
hz1, hz2, switch, stop = map(float, sys.argv[3:7])
phase, block, switched = 0.0, (c.c_int16 * 960)(), False
print("ready", time.time(), flush=True)
while time.time() < stop:
    hz = hz2 if time.time() >= switch else hz1
    if hz == hz2 and not switched:
        switched = True
        print("switched", time.time(), flush=True)
    for i in range(480):
        v = int(9000 * math.sin(phase))
        block[2 * i] = block[2 * i + 1] = v
        phase = (phase + 2 * math.pi * hz / 48000) % (2 * math.pi)
    if lib.pa_simple_write(s, block, c.sizeof(block), c.byref(error)) < 0:
        raise SystemExit("player write failed")
