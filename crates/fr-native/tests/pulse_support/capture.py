#!/usr/bin/env python3
"""Test-only independent real libpulse-simple null-sink monitor.
Read exactly one bounded second of synthetic audio, not a user's endpoint.
The parent owns/kills this process if a native read stalls. No PCM is logged.
"""
import ctypes as c
import pathlib
import sys

class Spec(c.Structure):
    _fields_ = [("format", c.c_int), ("rate", c.c_uint32), ("channels", c.c_uint8)]

class Attr(c.Structure):
    _fields_ = [(n, c.c_uint32) for n in ("maxlength", "tlength", "prebuf", "minreq", "fragsize")]

lib = c.CDLL("libpulse-simple.so.0")
lib.pa_simple_new.argtypes = [c.c_char_p, c.c_char_p, c.c_int, c.c_char_p,
                            c.c_char_p, c.POINTER(Spec), c.c_void_p,
                            c.POINTER(Attr), c.POINTER(c.c_int)]
lib.pa_simple_new.restype = c.c_void_p
lib.pa_simple_read.argtypes = [c.c_void_p, c.c_void_p, c.c_size_t, c.POINTER(c.c_int)]
lib.pa_simple_read.restype = c.c_int
lib.pa_simple_free.argtypes = [c.c_void_p]
lib.pa_simple_free.restype = None
spec = Spec(3, 48000, 2)
attr = Attr(19200, 0, 0, 0, 1920)
error = c.c_int()
stream = lib.pa_simple_new(("unix:" + sys.argv[1]).encode(), b"fr-test-monitor", 2,
                          b"fr_test.monitor", b"synthetic-fixture", c.byref(spec),
                          None, c.byref(attr), c.byref(error))
if not stream:
    raise RuntimeError("independent native monitor connection failed")
output = pathlib.Path(sys.argv[2])
try:
    output.with_suffix(".ready").touch(exist_ok=False)
    block = (c.c_int16 * 960)()
    with output.open("xb") as file:
        for _ in range(100):
            if lib.pa_simple_read(stream, block, c.sizeof(block), c.byref(error)) < 0:
                raise RuntimeError("independent native monitor read failed")
            file.write(bytes(block))
finally:
    lib.pa_simple_free(stream)
