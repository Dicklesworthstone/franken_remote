#!/usr/bin/env python3
"""Find the constant local clipboard UI title on a private Xvfb test display."""
import ctypes as c
import sys

x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong

def signature(name, result, *args):
    f = getattr(x, name)
    f.restype, f.argtypes = result, list(args)

signature("XOpenDisplay", D, c.c_char_p)
signature("XCloseDisplay", c.c_int, D)
signature("XDefaultRootWindow", W, D)
signature("XQueryTree", c.c_int, D, W, c.POINTER(W), c.POINTER(W), c.POINTER(c.POINTER(W)), c.POINTER(c.c_uint))
signature("XFetchName", c.c_int, D, W, c.POINTER(c.c_void_p))
signature("XFree", c.c_int, D)
d = x.XOpenDisplay(sys.argv[1].encode("ascii"))
assert d
try:
    root, parent, children, count = W(), W(), c.POINTER(W)(), c.c_uint()
    assert x.XQueryTree(d, x.XDefaultRootWindow(d), c.byref(root), c.byref(parent), c.byref(children), c.byref(count))
    assert count.value <= 128
    matches = []
    try:
        for i in range(count.value):
            name = c.c_void_p()
            if x.XFetchName(d, children[i], c.byref(name)) and name:
                try:
                    if c.string_at(name) == b"FrankenRemote - Clipboard sharing":
                        matches.append(children[i])
                finally:
                    x.XFree(name)
    finally:
        if children:
            x.XFree(children)
    assert len(matches) == 1
    print(matches[0])
finally:
    x.XCloseDisplay(d)
