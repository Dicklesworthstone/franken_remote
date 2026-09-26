#!/usr/bin/env python3
"""Independent Xlib/XTest integration peer. Test-only; not local-user evidence."""
import ctypes as c
import sys
import time

x = c.CDLL("libX11.so.6")
xt = c.CDLL("libXtst.so.6")

def signature(lib, name, result, *args):
    f = getattr(lib, name)
    f.restype, f.argtypes = result, list(args)
    return f

D, W = c.c_void_p, c.c_ulong
signature(x, "XOpenDisplay", D, c.c_char_p)
signature(x, "XCloseDisplay", c.c_int, D)
signature(x, "XDefaultRootWindow", W, D)
signature(x, "XDefaultScreen", c.c_int, D)
signature(x, "XSync", c.c_int, D, c.c_int)
signature(x, "XTranslateCoordinates", c.c_int, D, W, W, c.c_int, c.c_int,
          c.POINTER(c.c_int), c.POINTER(c.c_int), c.POINTER(W))
signature(x, "XUnmapWindow", c.c_int, D, W)
signature(x, "XDestroyWindow", c.c_int, D, W)
signature(x, "XResizeWindow", c.c_int, D, W, c.c_uint, c.c_uint)
signature(x, "XCreateSimpleWindow", W, D, W, c.c_int, c.c_int,
          c.c_uint, c.c_uint, c.c_uint, W, W)
signature(x, "XMapRaised", c.c_int, D, W)
signature(x, "XSetInputFocus", c.c_int, D, W, c.c_int, W)
signature(x, "XKeysymToKeycode", c.c_ubyte, D, W)
signature(xt, "XTestFakeMotionEvent", c.c_int, D, c.c_int, c.c_int, c.c_int, W)
signature(xt, "XTestFakeButtonEvent", c.c_int, D, c.c_uint, c.c_int, W)
signature(xt, "XTestFakeKeyEvent", c.c_int, D, c.c_uint, c.c_int, W)
signature(x, "XGetImage", D, D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, W, c.c_int)
signature(x, "XGetPixel", W, D, c.c_int, c.c_int)
signature(x, "XDestroyImage", c.c_int, D)

if len(sys.argv) < 4:
    raise SystemExit("usage: peer.py DISPLAY WINDOW OP [ARG]")
d = x.XOpenDisplay(sys.argv[1].encode("ascii"))
if not d:
    raise SystemExit("test peer display unavailable")
w, op = int(sys.argv[2]), sys.argv[3]
# The explicit device-* operations attribute events to Xvfb's non-XTEST slave.
# This is a fixture for local hardware, not protection against same-user X clients.
device_input = op.startswith("device-")
if device_input:
    op = op.removeprefix("device-")
    xi = c.CDLL("libXi.so.6")
    class DeviceInfo(c.Structure):
        _fields_ = [("id", W), ("type", W), ("name", c.c_char_p),
                    ("classes", c.c_int), ("use", c.c_int), ("info", D)]
    listed = signature(xi, "XListInputDevices", c.POINTER(DeviceInfo), D, c.POINTER(c.c_int))
    free = signature(xi, "XFreeDeviceList", None, c.POINTER(DeviceInfo))
    opened = signature(xi, "XOpenDevice", D, D, W)
    closed = signature(xi, "XCloseDevice", c.c_int, D, D)
    count = c.c_int()
    devices = listed(d, c.byref(count))
    assert devices and 0 < count.value <= 256
    name = b"Xvfb keyboard" if op == "key" else b"Xvfb mouse"
    ids = [devices[i].id for i in range(count.value) if devices[i].name == name]
    free(devices)
    assert len(ids) == 1, "explicit non-XTEST Xvfb device required"
    device = opened(d, ids[0])
    assert device
    fake = signature(xt, "XTestFakeDeviceKeyEvent" if op == "key" else "XTestFakeDeviceButtonEvent",
                     c.c_int, D, D, c.c_uint, c.c_int, c.POINTER(c.c_int), c.c_int, W)
def button(code, pressed):
    return fake(d, device, code, pressed, None, 0, 0) if device_input else xt.XTestFakeButtonEvent(d, code, pressed, 0)
def key_event(code, pressed):
    return fake(d, device, code, pressed, None, 0, 0) if device_input else xt.XTestFakeKeyEvent(d, code, pressed, 0)
try:
    root = x.XDefaultRootWindow(d)
    px, py, child = c.c_int(), c.c_int(), W()
    if op in ("click", "miss", "cover", "allow", "synthetic-allow", "release-allow", "drag-out", "press-allow"):
        assert x.XTranslateCoordinates(d, w, root, 0, 0, c.byref(px), c.byref(py), c.byref(child))
    if op in ("click", "miss"):
        dx, dy = (50, 95) if op == "click" else (6, 6)
        assert xt.XTestFakeMotionEvent(d, x.XDefaultScreen(d), px.value + dx, py.value + dy, 0)
        assert button(1, 1)
        assert button(1, 0)
    elif op in ("allow", "release-allow", "drag-out", "press-allow"):
        assert xt.XTestFakeMotionEvent(d, x.XDefaultScreen(d), px.value + 350, py.value + 95, 0)
        if op != "release-allow":
            assert button(1, 1)
        if op == "drag-out":
            assert xt.XTestFakeMotionEvent(d, x.XDefaultScreen(d), px.value + 6, py.value + 6, 0)
        if op != "press-allow":
            assert button(1, 0)
    elif op == "synthetic-allow":
        class Button(c.Structure):
            _fields_ = [("type", c.c_int), ("serial", W), ("send_event", c.c_int),
                        ("display", D), ("window", W), ("root", W), ("subwindow", W),
                        ("time", W), ("x", c.c_int), ("y", c.c_int),
                        ("x_root", c.c_int), ("y_root", c.c_int),
                        ("state", c.c_uint), ("button", c.c_uint), ("same_screen", c.c_int)]
        class Event(c.Union):
            _fields_ = [("button", Button), ("pad", c.c_long * 24)]
        send = signature(x, "XSendEvent", c.c_int, D, W, c.c_int, c.c_long, c.POINTER(Event))
        for kind, mask in ((4, 1 << 2), (5, 1 << 3)):
            event = Event()
            event.button = Button(kind, 0, 1, d, w, root, 0, 0, 350, 95,
                                  px.value + 350, py.value + 95, 0, 1, 1)
            assert send(d, w, 0, 0, c.byref(event))  # deliver to creator even without core selection
    elif op == "key":
        key = int(sys.argv[4], 0) if len(sys.argv) > 4 else 0xff1b
        x.XSetInputFocus(d, w, 2, 0)
        code = x.XKeysymToKeycode(d, key)
        assert code
        assert key_event(code, 1)
        assert key_event(code, 0)
    elif op == "unmap":
        x.XUnmapWindow(d, w)
    elif op == "destroy":
        x.XDestroyWindow(d, w)
    elif op == "resize":
        x.XResizeWindow(d, w, 120, 40)
    elif op == "cover":
        other = x.XCreateSimpleWindow(d, root, px.value, py.value, 480, 148, 0, 0, 0)
        assert other
        x.XMapRaised(d, other)
        x.XSync(d, 0)
        time.sleep(0.15)
        x.XDestroyWindow(d, other)
    elif op == "snapshot":
        image = x.XGetImage(d, w, 0, 0, 480, 148, W(-1), 2)
        assert image
        try:
            data = bytearray()
            for yy in range(148):
                for xx in range(480):
                    rgb = x.XGetPixel(image, xx, yy)
                    data.extend(((rgb >> 16) & 255, (rgb >> 8) & 255, rgb & 255))
            # Assert a real label and button were drawn, not just a mapped blank.
            assert data.count(0) > 2000 and data.count(255) > 100000
            if len(sys.argv) > 4:
                with open(sys.argv[4], "wb") as output:
                    output.write(b"P6\n480 148\n255\n" + data)
        finally:
            x.XDestroyImage(image)
    else:
        raise SystemExit("unknown test operation")
    x.XSync(d, 0)
finally:
    if device_input:
        closed(d, device)
    x.XCloseDisplay(d)
