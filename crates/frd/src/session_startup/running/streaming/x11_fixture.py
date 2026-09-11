#!/usr/bin/env python3
"""Test-only root painter/readback; no production socket or authority handling."""
import ctypes as c
import sys
x = c.CDLL('libX11.so.6')
for name, args, result in [
    ('XOpenDisplay', [c.c_char_p], c.c_void_p),
    ('XDefaultRootWindow', [c.c_void_p], c.c_ulong),
    ('XCreateGC', [c.c_void_p, c.c_ulong, c.c_ulong, c.c_void_p], c.c_void_p),
    ('XSetForeground', [c.c_void_p, c.c_void_p, c.c_ulong], c.c_int),
    ('XFillRectangle', [c.c_void_p, c.c_ulong, c.c_void_p, c.c_int, c.c_int, c.c_uint, c.c_uint], c.c_int),
    ('XSync', [c.c_void_p, c.c_int], c.c_int),
    ('XGetImage', [c.c_void_p, c.c_ulong, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_ulong, c.c_int], c.c_void_p),
    ('XGetPixel', [c.c_void_p, c.c_int, c.c_int], c.c_ulong),
    ('XDestroyImage', [c.c_void_p], c.c_int),
    ('XFreeGC', [c.c_void_p, c.c_void_p], c.c_int),
    ('XCloseDisplay', [c.c_void_p], c.c_int),
]:
    getattr(x, name).argtypes = args
    getattr(x, name).restype = result
display = x.XOpenDisplay(sys.argv[1].encode())
assert display
root = x.XDefaultRootWindow(display)
gc = x.XCreateGC(display, root, 0, None)
assert gc
print('ready', flush=True)
try:
    for line in sys.stdin:
        fields = line.split()
        if fields == ['read']:
            image = x.XGetImage(display, root, 163, 123, 1, 1, c.c_ulong(-1).value, 2)
            assert image
            try:
                print(x.XGetPixel(image, 0, 0) & 0xffffff, flush=True)
            finally:
                x.XDestroyImage(image)
        else:
            assert len(fields) == 2 and fields[0] == 'paint'
            pixel = int(fields[1])
            assert 0 <= pixel <= 0xffffff
            x.XSetForeground(display, gc, pixel)
            x.XFillRectangle(display, root, gc, 0, 0, 320, 240)
            x.XSync(display, 0)
            print('painted', flush=True)
finally:
    x.XFreeGC(display, gc)
    x.XCloseDisplay(display)
