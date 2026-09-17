#!/usr/bin/env python3
"""Wait for a mapped local chooser, then instrument a real XTest gesture."""
import ctypes as c
import pathlib
import runpy
import sys
import time
x = c.CDLL('libX11.so.6')
D, W = c.c_void_p, c.c_ulong

def sig(name, result, *args):
    f = getattr(x, name)
    f.restype, f.argtypes = result, list(args)

sig('XOpenDisplay', D, c.c_char_p)
sig('XCloseDisplay', c.c_int, D)
sig('XDefaultRootWindow', W, D)
sig('XQueryTree', c.c_int, D, W, c.POINTER(W), c.POINTER(W), c.POINTER(c.POINTER(W)), c.POINTER(c.c_uint))
sig('XFree', c.c_int, D)
sig('XGetGeometry', c.c_int, D, W, c.POINTER(W), c.POINTER(c.c_int), c.POINTER(c.c_int), c.POINTER(c.c_uint), c.POINTER(c.c_uint), c.POINTER(c.c_uint), c.POINTER(c.c_uint))
d = x.XOpenDisplay(sys.argv[1].encode('ascii'))
assert d
window = None
try:
    until = time.monotonic() + 3
    while window is None:
        assert time.monotonic() < until, 'no native picker within fixture deadline'
        root, parent, count = W(), W(), c.c_uint()
        children = c.POINTER(W)()
        assert x.XQueryTree(d, x.XDefaultRootWindow(d), c.byref(root), c.byref(parent), c.byref(children), c.byref(count))
        assert count.value <= 8, 'unexpected unbounded native windows'
        try:
            for i in range(count.value):
                xx, yy = c.c_int(), c.c_int()
                width, height, border, depth = c.c_uint(), c.c_uint(), c.c_uint(), c.c_uint()
                assert x.XGetGeometry(d, children[i], c.byref(root), c.byref(xx), c.byref(yy), c.byref(width), c.byref(height), c.byref(border), c.byref(depth))
                if width.value == 560 and 132 <= height.value <= 384:
                    assert xx.value == 0 and yy.value == 0, 'fixture has no window manager'
                    window = children[i]
        finally:
            x.XFree(children)
        if window is None:
            time.sleep(.005)
    # Keep the catalog pending through multiple actual session/network turns.
    time.sleep(.08)
finally:
    x.XCloseDisplay(d)
print(window, flush=True)
sys.argv[2] = str(window)
runpy.run_path(str(pathlib.Path(__file__).with_name('picker_peer.py')), run_name='__main__')
