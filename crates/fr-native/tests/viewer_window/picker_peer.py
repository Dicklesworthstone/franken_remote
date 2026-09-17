#!/usr/bin/env python3
"""Independent Xlib/XTest driver for the bounded display-choice surface."""
import ctypes as c
import sys
x = c.CDLL("libX11.so.6")
t = c.CDLL("libXtst.so.6")
D, W = c.c_void_p, c.c_ulong

def sig(lib, name, result, *args):
    f = getattr(lib, name)
    f.restype, f.argtypes = result, list(args)
    return f

sig(x, "XOpenDisplay", D, c.c_char_p)
sig(x, "XCloseDisplay", c.c_int, D)
sig(x, "XDefaultRootWindow", W, D)
sig(x, "XSetInputFocus", c.c_int, D, W, c.c_int, W)
sig(x, "XKeysymToKeycode", c.c_uint8, D, W)
sig(x, "XSync", c.c_int, D, c.c_int)
sig(x, "XGetImage", D, D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, W, c.c_int)
sig(x, "XGetPixel", W, D, c.c_int, c.c_int)
sig(x, "XDestroyImage", c.c_int, D)
sig(x, "XSendEvent", c.c_int, D, W, c.c_int, c.c_long, D)
sig(t, "XTestFakeMotionEvent", c.c_int, D, c.c_int, c.c_int, c.c_int, W)
sig(t, "XTestFakeButtonEvent", c.c_int, D, c.c_uint, c.c_int, W)
sig(t, "XTestFakeKeyEvent", c.c_int, D, c.c_uint, c.c_int, W)
class Button(c.Structure):
    _fields_ = [("type", c.c_int), ("serial", W), ("send_event", c.c_int),
                ("display", D), ("window", W), ("root", W), ("subwindow", W),
                ("time", W), ("x", c.c_int), ("y", c.c_int),
                ("x_root", c.c_int), ("y_root", c.c_int),
                ("state", c.c_uint), ("button", c.c_uint), ("same_screen", c.c_int)]
class Event(c.Union):
    _fields_ = [("button", Button), ("pad", c.c_long * 24)]
d = x.XOpenDisplay(sys.argv[1].encode("ascii"))
assert d
w, op = int(sys.argv[2]), sys.argv[3]
def key(symbol):
    code = x.XKeysymToKeycode(d, symbol)
    assert code
    assert t.XTestFakeKeyEvent(d, code, 1, 0)
    assert t.XTestFakeKeyEvent(d, code, 0, 0)
def move(row):
    assert t.XTestFakeMotionEvent(d, 0, 40, 64 + row * 36 + 10, 0)
def button(press):
    assert t.XTestFakeButtonEvent(d, 1, press, 0)
try:
    if op == "pixels":
        image = x.XGetImage(d, w, 0, 0, 560, 384, W(-1), 2)
        assert image
        try:
            for row in range(8):
                black = sum(x.XGetPixel(image, xx, yy) == 0
                            for yy in range(64 + row * 36, 64 + row * 36 + 28)
                            for xx in range(12, 420))
                assert black > 100, (row, black)
            print("drawn")
        finally:
            x.XDestroyImage(image)
    elif op.startswith("click-"):
        move(int(op.split("-")[1])); button(1); button(0)
    elif op == "cross-row":
        move(0); button(1); move(1); button(0)
    elif op == "release-only":
        move(0); button(0)
    elif op == "synthetic":
        for typ, mask in [(4, 1 << 2), (5, 1 << 3)]:
            e = Event()
            e.button.type, e.button.display, e.button.window = typ, d, w
            e.button.root = x.XDefaultRootWindow(d)
            e.button.x, e.button.y, e.button.button, e.button.same_screen = 40, 74, 1, 1
            assert x.XSendEvent(d, w, 0, mask, c.byref(e))
    else:
        x.XSetInputFocus(d, w, 1, 0)
        if op == "enter-only": key(0xff0d)
        elif op == "second-key": key(0xff54); key(0xff54); key(0xff0d)
        elif op == "digit-2": key(0x32)
        elif op == "escape": key(0xff1b)
        else: raise AssertionError(op)
    x.XSync(d, 0)
finally:
    x.XCloseDisplay(d)
