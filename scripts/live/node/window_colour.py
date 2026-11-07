# Fraction of a 64x36 sample grid over WINDOW (or the root) within FUZZ of RRGGBB.
# Usage: window_colour.py DISPLAY WINDOW|root RRGGBB [FUZZ [SOURCE_WxH]]
# With SOURCE_WxH the grid covers only the rectangle where the viewer presents a
# picture of that size, so a fitted viewer is sampled at the host grid's points.
import ctypes as c, sys
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = D
x.XDefaultRootWindow.restype = W; x.XDefaultRootWindow.argtypes = [D]
x.XGetGeometry.argtypes = [D, W, c.POINTER(W), c.POINTER(c.c_int), c.POINTER(c.c_int),
                           c.POINTER(c.c_uint), c.POINTER(c.c_uint), c.POINTER(c.c_uint), c.POINTER(c.c_uint)]
x.XGetImage.restype = c.c_void_p
x.XGetImage.argtypes = [D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_ulong, c.c_int]
x.XGetPixel.restype = c.c_ulong
x.XGetPixel.argtypes = [c.c_void_p, c.c_int, c.c_int]
x.XDestroyImage.argtypes = [c.c_void_p]

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
w = x.XDefaultRootWindow(d) if sys.argv[2] == "root" else int(sys.argv[2], 0)
want = int(sys.argv[3], 16)
fuzz = int(sys.argv[4]) if len(sys.argv) > 4 else 24
root, px, py = W(), c.c_int(), c.c_int()
width, height, border, depth = c.c_uint(), c.c_uint(), c.c_uint(), c.c_uint()
assert x.XGetGeometry(d, w, c.byref(root), c.byref(px), c.byref(py), c.byref(width),
                      c.byref(height), c.byref(border), c.byref(depth))
image = x.XGetImage(d, w, 0, 0, width.value, height.value, 0xFFFFFFFF, 2)
assert image, "XGetImage"
ox, oy, fw, fh = 0, 0, width.value, height.value
if len(sys.argv) > 5:
    sw, sh = (int(v) for v in sys.argv[5].split("x"))
    ox, oy, fw, fh = fitted(sw, sh, width.value, height.value)
near = total = 0
for gy in range(36):
    for gx in range(64):
        p = x.XGetPixel(image, ox + gx * fw // 64, oy + gy * fh // 36)
        total += 1
        near += all(abs(((p >> s) & 255) - ((want >> s) & 255)) <= fuzz for s in (16, 8, 0))
x.XDestroyImage(image)
print(f"{near}/{total} {width.value}x{height.value}")
