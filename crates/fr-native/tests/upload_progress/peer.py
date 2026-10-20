#!/usr/bin/env python3
"""Independent Xlib/XTest driver for the production upload panel. No third-party Python modules."""
import ctypes as C
import sys
import time

x = C.CDLL("libX11.so.6")
t = C.CDLL("libXtst.so.6")
D = C.c_void_p
U = C.c_ulong
I = C.c_int

def bind(lib, name, result, *args):
    f = getattr(lib, name); f.restype = result; f.argtypes = args
    return f

open_display = bind(x, "XOpenDisplay", D, C.c_char_p)
root_window = bind(x, "XDefaultRootWindow", U, D)
create = bind(x, "XCreateSimpleWindow", U, D, U, I, I, C.c_uint, C.c_uint, C.c_uint, U, U)
map_window = bind(x, "XMapWindow", I, D, U)
focus = bind(x, "XSetInputFocus", I, D, U, I, U)
get_focus = bind(x, "XGetInputFocus", I, D, C.POINTER(U), C.POINTER(I))
sync = bind(x, "XSync", I, D, I)
atom = bind(x, "XInternAtom", U, D, C.c_char_p, I)
change_prop = bind(x, "XChangeProperty", I, D, U, U, U, I, I, D, I)
get_prop = bind(x, "XGetWindowProperty", I, D, U, U, C.c_long, C.c_long, I, U,
                C.POINTER(U), C.POINTER(I), C.POINTER(U), C.POINTER(U), C.POINTER(D))
query = bind(x, "XQueryTree", I, D, U, C.POINTER(U), C.POINTER(U), C.POINTER(C.POINTER(U)), C.POINTER(C.c_uint))
free = bind(x, "XFree", I, D)
warp = bind(x, "XWarpPointer", I, D, U, U, I, I, C.c_uint, C.c_uint, I, I)
button = bind(t, "XTestFakeButtonEvent", I, D, C.c_uint, I, U)
unmap = bind(x, "XUnmapWindow", I, D, U)
resize = bind(x, "XResizeWindow", I, D, U, C.c_uint, C.c_uint)
send = bind(x, "XSendEvent", I, D, U, I, C.c_long, D)
get_image = bind(x, "XGetImage", D, D, U, I, I, C.c_uint, C.c_uint, U, I)
pixel = bind(x, "XGetPixel", U, D, I, I)
destroy_image = bind(x, "XDestroyImage", I, D)
close = bind(x, "XCloseDisplay", I, D)

class Button(C.Structure):
    _fields_ = [("type", I), ("serial", U), ("send_event", I), ("display", D),
        ("window", U), ("root", U), ("subwindow", U), ("time", U),
        ("x", I), ("y", I), ("x_root", I), ("y_root", I),
        ("state", C.c_uint), ("button", C.c_uint), ("same_screen", I)]
class Message(C.Structure):
    _fields_ = [("type", I), ("serial", U), ("send_event", I), ("display", D),
        ("window", U), ("message_type", U), ("format", I), ("data", C.c_long * 5)]
class Event(C.Union):
    _fields_ = [("button", Button), ("message", Message), ("pad", C.c_long * 24)]

class Attributes(C.Structure):
    _fields_ = [("x", I), ("y", I), ("width", I), ("height", I), ("border_width", I), ("depth", I),
        ("visual", D), ("root", U), ("klass", I), ("bit_gravity", I), ("win_gravity", I),
        ("backing_store", I), ("backing_planes", U), ("backing_pixel", U), ("save_under", I),
        ("colormap", U), ("map_installed", I), ("map_state", I), ("all_event_masks", C.c_long),
        ("your_event_mask", C.c_long), ("do_not_propagate_mask", C.c_long), ("override_redirect", I), ("screen", D)]
attributes = bind(x, "XGetWindowAttributes", I, D, U, C.POINTER(Attributes))
def viewable(window):
    a = Attributes()
    return bool(window and attributes(d, window, C.byref(a)) and a.map_state == 2)

d = open_display(sys.argv[1].encode())
assert d, "X display unavailable"
root = root_window(d)
parent = create(d, root, 680, 20, 320, 240, 0, 0, 0xffffff)
# Match the real fixed-size viewer rather than an unconstrained toy parent.
size = (C.c_long * 18)(); size[0] = (1 << 4) | (1 << 5)
size[5] = size[7] = 320; size[6] = size[8] = 240
change_prop(d, parent, atom(d, b"WM_NORMAL_HINTS", 0), atom(d, b"WM_SIZE_HINTS", 0), 32, 0, size, 18)
map_window(d, parent); sync(d, 0)
until = time.monotonic() + 3
while not viewable(parent):
    assert time.monotonic() < until, "test parent was not mapped"
    time.sleep(0.01)
focus(d, parent, 1, 0); sync(d, 0)
print(parent, flush=True)

def prop(window, name):
    typ, fmt, count, rest, data = U(), I(), U(), U(), D()
    assert get_prop(d, window, atom(d, name, 0), 0, 256, 0, 0,
        C.byref(typ), C.byref(fmt), C.byref(count), C.byref(rest), C.byref(data)) == 0
    if not data: return None
    try:
        if fmt.value == 8: return C.string_at(data, count.value)
        if fmt.value == 32: return list(C.cast(data, C.POINTER(U))[:count.value])
        return None
    finally: free(data)

def panel(window=None, depth=0):
    if window is None: window = root
    if depth > 3: return 0
    r, p, children, count = U(), U(), C.POINTER(U)(), C.c_uint()
    assert query(d, window, C.byref(r), C.byref(p), C.byref(children), C.byref(count))
    try:
        candidates = list(children[:count.value]) if children else []
    finally:
        if children: free(children)
    for child in candidates:
        if prop(child, b"WM_TRANSIENT_FOR") == [parent] and prop(child, b"WM_CLASS") == b"franken-remote-upload\0FrankenRemote\0":
            return child
        found = panel(child, depth + 1)
        if found: return found
    return 0

def point(name):
    return {"pause": (100, 250), "resume": (304, 250), "cancel": (508, 250), "outside": (200, 220)}[name]

for raw in sys.stdin:
    parts = raw.split()
    if not parts: continue
    command = parts[0]
    if command == "quit": break
    w = panel()
    result = "ok"
    if command == "exists": result = "yes" if w else "no"
    elif command == "mapped": result = "yes" if viewable(w) else "no"
    elif command == "focus":
        f, revert = U(), I(); get_focus(d, C.byref(f), C.byref(revert))
        result = "parent" if f.value == parent else "other"
    elif command == "hints":
        h = prop(w, b"WM_HINTS")
        result = "nonfocus" if h and h[0] & 1 and h[1] == 0 else "invalid"
    elif command in ("click", "press", "release"):
        xx, yy = point(parts[1]); warp(d, 0, w, 0, 0, 0, 0, xx, yy)
        if command in ("click", "press"): button(d, 1, 1, 0)
        if command in ("click", "release"): button(d, 1, 0, 0)
    elif command == "synthetic":
        for typ in (4, 5):
            e = Event(); e.button = Button(typ, 0, 1, d, w, root, 0, 0, 100, 250, 100, 250, 0, 1, 1)
            send(d, w, 0, 4 if typ == 4 else 8, C.byref(e))
    elif command == "close":
        e = Event(); e.message.type = 33; e.message.display = d; e.message.window = w
        e.message.message_type = atom(d, b"WM_PROTOCOLS", 0); e.message.format = 32
        e.message.data[0] = atom(d, b"WM_DELETE_WINDOW", 0)
        send(d, w, 0, 0, C.byref(e))
    elif command == "unmap": unmap(d, w)
    elif command == "parent_unmap": unmap(d, parent)
    elif command == "resize": resize(d, w, 638, 308)
    elif command == "bar":
        image = get_image(d, w, 18, 205, 602, 1, U(-1).value, 2)
        assert image
        try: result = str(sum(pixel(image, i, 0) == 0 for i in range(602)))
        finally: destroy_image(image)
    elif command == "flood":
        for _ in range(80):
            e = Event(); e.message.type = 33; e.message.window = w; e.message.format = 32
            send(d, w, 0, 0, C.byref(e))
    else: raise AssertionError(command)
    sync(d, 0); print(result, flush=True)
close(d)
