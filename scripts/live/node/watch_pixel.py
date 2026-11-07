# Poll the FrankenRemote viewer pixel that presents host point (HX, HY) of a
# SOURCE_WxH host picture, and print the wall-clock epoch at which it first shows
# each colour of SEQUENCE in turn (fuzz 24 per channel).
# Usage: watch_pixel.py DISPLAY WINDOW HX HY SOURCE_WxH TIMEOUT_S RRGGBB...
import ctypes as c, sys, time
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = D
x.XGetImage.restype = c.c_void_p
x.XGetImage.argtypes = [D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_ulong, c.c_int]
x.XGetPixel.restype = c.c_ulong
x.XGetPixel.argtypes = [c.c_void_p, c.c_int, c.c_int]
x.XDestroyImage.argtypes = [c.c_void_p]
x.XGetGeometry.argtypes = [D, W, c.POINTER(W), c.POINTER(c.c_int), c.POINTER(c.c_int),
                           c.POINTER(c.c_uint), c.POINTER(c.c_uint), c.POINTER(c.c_uint), c.POINTER(c.c_uint)]

def fitted(sw, sh, tw, th):
    """Where the viewer presents an SWxSH picture in a TWxTH window: aspect kept,
    floor division, centred (fr-media worker/presentation.rs Fit::new)."""
    if tw * sh <= th * sw:
        width, height = tw, tw * sh // sw
    else:
        width, height = th * sw // sh, th
    return (tw - width) // 2, (th - height) // 2, width, height


d = x.XOpenDisplay(sys.argv[1].encode())
assert d, "display"
w, hx, hy = int(sys.argv[2], 0), int(sys.argv[3]), int(sys.argv[4])
sw, sh = (int(v) for v in sys.argv[5].split("x"))
until = time.time() + float(sys.argv[6])
root, gx, gy = W(), c.c_int(), c.c_int()
width, height, border, depth = c.c_uint(), c.c_uint(), c.c_uint(), c.c_uint()
assert x.XGetGeometry(d, w, c.byref(root), c.byref(gx), c.byref(gy), c.byref(width),
                      c.byref(height), c.byref(border), c.byref(depth))
ox, oy, fw, fh = fitted(sw, sh, width.value, height.value)
px, py = ox + hx * fw // sw, oy + hy * fh // sh
print(f"# host ({hx},{hy}) of {sw}x{sh} -> viewer ({px},{py}) of {width.value}x{height.value}",
      file=sys.stderr, flush=True)
def near(p, want):
    return all(abs(((p >> s) & 255) - ((want >> s) & 255)) <= 24 for s in (16, 8, 0))
for want in (int(v, 16) for v in sys.argv[7:]):
    seen = None
    while seen is None and time.time() < until:
        image = x.XGetImage(d, w, px, py, 1, 1, 0xFFFFFFFF, 2)
        if image:
            if near(x.XGetPixel(image, 0, 0), want):
                seen = time.time()
            x.XDestroyImage(image)
        if seen is None:
            time.sleep(0.002)
    print(f"seen {want:06x} {seen}", flush=True)
