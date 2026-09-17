"""Native test peer only: ordered X11 geometry/visibility changes."""
import ctypes as c
import sys
x = c.CDLL("libX11.so.6")
x.XOpenDisplay.argtypes = [c.c_char_p]
x.XOpenDisplay.restype = c.c_void_p
for name, tail in [("XResizeWindow", [c.c_uint, c.c_uint]), ("XUnmapWindow", []), ("XMapWindow", [])]:
    getattr(x, name).argtypes = [c.c_void_p, c.c_ulong] + tail
x.XSync.argtypes = [c.c_void_p, c.c_int]
x.XCloseDisplay.argtypes = [c.c_void_p]
d = x.XOpenDisplay(sys.argv[1].encode("ascii"))
assert d
w = int(sys.argv[2])
if sys.argv[3] == "resize-return":
    x.XResizeWindow(d, w, 322, 240)
    x.XResizeWindow(d, w, 320, 240)
elif sys.argv[3] == "unmap-return":
    x.XUnmapWindow(d, w)
    x.XMapWindow(d, w)
else:
    raise ValueError("unknown mutation")
x.XSync(d, 0)
x.XCloseDisplay(d)
