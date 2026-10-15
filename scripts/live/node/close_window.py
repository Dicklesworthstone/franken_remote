# Ask window WINDOW on DISPLAY to close the way a window manager does
# (WM_PROTOCOLS / WM_DELETE_WINDOW). Usage: close_window.py DISPLAY WINDOW
import ctypes as c, sys
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = D
x.XInternAtom.restype = W; x.XInternAtom.argtypes = [D, c.c_char_p, c.c_int]
x.XSendEvent.argtypes = [D, W, c.c_int, c.c_long, c.c_void_p]
x.XSync.argtypes = [D, c.c_int]
class Client(c.Structure):
    _fields_ = [("type", c.c_int), ("serial", c.c_ulong), ("synthetic", c.c_int), ("display", D),
                ("window", W), ("message_type", W), ("format", c.c_int), ("data", c.c_long * 5)]
class Event(c.Union):
    _fields_ = [("client", Client), ("pad", c.c_long * 24)]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d, "display"
e = Event()
e.client.type = 33  # ClientMessage
e.client.display = d
e.client.window = int(sys.argv[2])
e.client.message_type = x.XInternAtom(d, b"WM_PROTOCOLS", 0)
e.client.format = 32
e.client.data[0] = x.XInternAtom(d, b"WM_DELETE_WINDOW", 0)
assert x.XSendEvent(d, int(sys.argv[2]), 0, 0, c.byref(e))
x.XSync(d, 0)
