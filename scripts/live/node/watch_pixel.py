# Poll one pixel of the FrankenRemote viewer window and print the wall-clock
# epoch at which it first shows each colour of SEQUENCE in turn (fuzz 24 per
# channel). Usage: watch_pixel.py DISPLAY WINDOW X Y TIMEOUT_S RRGGBB...
import ctypes as c, sys, time
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = D
x.XGetImage.restype = c.c_void_p
x.XGetImage.argtypes = [D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_ulong, c.c_int]
x.XGetPixel.restype = c.c_ulong
x.XGetPixel.argtypes = [c.c_void_p, c.c_int, c.c_int]
x.XDestroyImage.argtypes = [c.c_void_p]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d, "display"
w, px, py, until = int(sys.argv[2], 0), int(sys.argv[3]), int(sys.argv[4]), time.time() + float(sys.argv[5])
def near(p, want):
    return all(abs(((p >> s) & 255) - ((want >> s) & 255)) <= 24 for s in (16, 8, 0))
for want in (int(v, 16) for v in sys.argv[6:]):
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
