# Record a private sink's monitor until STOP_EPOCH: each 20 ms block is stored
# as an 8-byte receive epoch (float64) then 3840 bytes of s16le stereo.
# Usage: recorder.py SERVER SINK STOP_EPOCH OUT
import ctypes as c, struct, sys, time
class Spec(c.Structure):
    _fields_ = [("format", c.c_int), ("rate", c.c_uint32), ("channels", c.c_uint8)]
class Attr(c.Structure):
    _fields_ = [(n, c.c_uint32) for n in ("maxlength", "tlength", "prebuf", "minreq", "fragsize")]
lib = c.CDLL("libpulse-simple.so.0")
lib.pa_simple_new.argtypes = [c.c_char_p, c.c_char_p, c.c_int, c.c_char_p, c.c_char_p,
                              c.POINTER(Spec), c.c_void_p, c.POINTER(Attr), c.POINTER(c.c_int)]
lib.pa_simple_new.restype = c.c_void_p
lib.pa_simple_read.argtypes = [c.c_void_p, c.c_void_p, c.c_size_t, c.POINTER(c.c_int)]
error = c.c_int()
s = lib.pa_simple_new(sys.argv[1].encode(), b"fr-live-recorder", 2, (sys.argv[2] + ".monitor").encode(),
                      b"live-monitor", c.byref(Spec(3, 48000, 2)), None,
                      c.byref(Attr(19200, 0, 0, 0, 3840)), c.byref(error))
assert s, "recorder connection failed"
stop = float(sys.argv[3])
block = (c.c_int16 * 1920)()
with open(sys.argv[4], "wb") as out:
    while time.time() < stop:
        if lib.pa_simple_read(s, block, c.sizeof(block), c.byref(error)) < 0:
            raise SystemExit("recorder read failed")
        out.write(struct.pack("<d", time.time()) + bytes(block))
