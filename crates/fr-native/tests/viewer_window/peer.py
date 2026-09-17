#!/usr/bin/env python3
"""Independent Xlib peer for the client-owned window. Test-only."""
import ctypes as c
import sys
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
def sig(name, result, *args):
    f = getattr(x, name)
    f.restype, f.argtypes = result, list(args)
    return f
sig("XOpenDisplay", D, c.c_char_p)
sig("XCloseDisplay", c.c_int, D)
sig("XDefaultRootWindow", W, D)
sig("XQueryTree", c.c_int, D, W, c.POINTER(W), c.POINTER(W), c.POINTER(c.POINTER(W)), c.POINTER(c.c_uint))
sig("XFree", c.c_int, D)
sig("XSync", c.c_int, D, c.c_int)
sig("XMoveWindow", c.c_int, D, W, c.c_int, c.c_int)
sig("XResizeWindow", c.c_int, D, W, c.c_uint, c.c_uint)
sig("XUnmapWindow", c.c_int, D, W)
sig("XDestroyWindow", c.c_int, D, W)
sig("XInternAtom", W, D, c.c_char_p, c.c_int)
sig("XSendEvent", c.c_int, D, W, c.c_int, c.c_long, D)
sig("XGetImage", D, D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, W, c.c_int)
sig("XGetPixel", W, D, c.c_int, c.c_int)
sig("XDestroyImage", c.c_int, D)
class Client(c.Structure):
    _fields_ = [("type", c.c_int), ("serial", c.c_ulong), ("synthetic", c.c_int),
                ("display", D), ("window", W), ("message_type", W),
                ("format", c.c_int), ("data", c.c_long * 5)]
class Event(c.Union):
    _fields_ = [("client", Client), ("pad", c.c_long * 24)]
d = x.XOpenDisplay(sys.argv[1].encode("ascii"))
assert d, "test display missing"
w, op = int(sys.argv[2]), sys.argv[3]
try:
    if op == "exists":
        root, parent, children, count = W(), W(), c.POINTER(W)(), c.c_uint()
        assert x.XQueryTree(d, x.XDefaultRootWindow(d), c.byref(root), c.byref(parent), c.byref(children), c.byref(count))
        try:
            print(int(w in [children[i] for i in range(count.value)]))
        finally:
            if children: x.XFree(children)
    elif op == "move": x.XMoveWindow(d, w, 30, 40)
    elif op == "resize": x.XResizeWindow(d, w, 322, 240)
    elif op == "unmap": x.XUnmapWindow(d, w)
    elif op == "destroy": x.XDestroyWindow(d, w)
    elif op == "close":
        e = Event()
        e.client.type = 33
        e.client.display = d
        e.client.window = w
        e.client.message_type = x.XInternAtom(d, b"WM_PROTOCOLS", 0)
        e.client.format = 32
        e.client.data[0] = x.XInternAtom(d, b"WM_DELETE_WINDOW", 0)
        assert x.XSendEvent(d, w, 0, 0, c.byref(e))
    elif op == "pixel":
        image = x.XGetImage(d, w, 0, 0, 1, 1, W(-1), 2)
        assert image
        try: print(x.XGetPixel(image, 0, 0))
        finally: x.XDestroyImage(image)
    else: raise AssertionError("unknown operation")
    x.XSync(d, 0)
finally: x.XCloseDisplay(d)
