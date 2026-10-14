"""Independent Xlib selection peer. No FrankenRemote code or content logging.

Models owners with no TIMESTAMP target (including xclip's UTF8 response to
unknown targets). All text still crosses the real X server. Commands and
responses are bounded test-only control; no peer is a production dependency.
"""
import ctypes as c
import select
import sys

x = c.CDLL("libX11.so.6")
W, I, U, L, D = c.c_ulong, c.c_int, c.c_uint, c.c_long, c.c_void_p

def sig(name, result, *args):
    f = getattr(x, name)
    f.restype, f.argtypes = result, args
    return f

sig("XOpenDisplay", D, c.c_char_p)
sig("XDefaultRootWindow", W, D)
sig("XCreateSimpleWindow", W, D, W, I, I, U, U, U, W, W)
sig("XInternAtom", W, D, c.c_char_p, I)
sig("XSelectInput", I, D, W, L)
sig("XSetSelectionOwner", I, D, W, W, W)
sig("XGetSelectionOwner", W, D, W)
sig("XChangeProperty", I, D, W, W, W, I, I, c.c_void_p, I)
sig("XSendEvent", I, D, W, I, L, c.c_void_p)
sig("XSync", I, D, I)
sig("XFlush", I, D)
sig("XPending", I, D)
sig("XNextEvent", I, D, c.c_void_p)
sig("XConnectionNumber", I, D)
sig("XCloseDisplay", I, D)

class Request(c.Structure):
    _fields_ = [("type", I), ("serial", W), ("sent", I), ("display", D),
                ("owner", W), ("requestor", W), ("selection", W),
                ("target", W), ("property", W), ("time", W)]

class Notify(c.Structure):
    _fields_ = [("type", I), ("serial", W), ("sent", I), ("display", D),
                ("requestor", W), ("selection", W), ("target", W),
                ("property", W), ("time", W)]

class Property(c.Structure):
    _fields_ = [("type", I), ("serial", W), ("sent", I), ("display", D),
                ("window", W), ("atom", W), ("time", W), ("state", I)]

class Event(c.Union):
    _fields_ = [("type", I), ("request", Request), ("notify", Notify),
                ("prop", Property), ("pad", L * 24)]

# Cancellation destroys the reader's private requestor, which can race a reply.
# Ignore only BadWindow; all other X errors fail the fixture.
class XError(c.Structure):
    _fields_ = [("type", I), ("display", D), ("resource", W), ("serial", W),
                ("code", c.c_ubyte), ("major", c.c_ubyte), ("minor", c.c_ubyte)]
ERROR = c.CFUNCTYPE(I, D, c.POINTER(XError))
errors = []
@ERROR
def on_error(_display, error):
    if error.contents.code != 3:
        errors.append(error.contents.code)
    return 0
sig("XSetErrorHandler", c.c_void_p, ERROR)(on_error)
d = x.XOpenDisplay(None)
assert d, "test display unavailable"
w = x.XCreateSimpleWindow(d, x.XDefaultRootWindow(d), 0, 0, 1, 1, 0, 0, 0)
x.XSelectInput(d, w, 1 << 22)
clipboard, utf8, timestamp, clock, incr = [x.XInternAtom(d, n, 0) for n in
    (b"CLIPBOARD", b"UTF8_STRING", b"TIMESTAMP", b"FR_TEST_CLOCK", b"INCR")]
mode = sys.argv[1]
text, acquired, requests = b"", 0, 0
transfers = {}

def event(e):
    global requests
    if e.type == 30:
        r = e.request
        prop = r.property or r.target
        requests += 1
        if r.target == timestamp and mode == "refuse":
            prop = 0
        elif len(text) > 16384 or mode.startswith("unknown"):
            # xclip omits the advisory lower-bound word; malformed variants
            # verify that this compatibility does not admit arbitrary headers.
            x.XSelectInput(d, r.requestor, 1 << 22)
            words = 0 if mode.startswith("unknown") else 1
            fmt = 8 if mode == "unknown-bad-format" else 32
            if mode == "unknown-extra":
                words = 2
            x.XChangeProperty(d, r.requestor, prop, incr, fmt, 0,
                              (W * 2)(len(text), len(text)), words)
            transfers[(r.requestor, prop)] = (text, 0)
        else:
            # xclip treats unknown targets like its selected UTF8 target.
            x.XChangeProperty(d, r.requestor, prop, utf8, 8, 0,
                              c.c_char_p(text), len(text))
        reply = Event()
        reply.notify = Notify(31, 0, 1, d, r.requestor, r.selection,
                              r.target, prop, r.time)
        x.XSendEvent(d, r.requestor, 0, 0, c.byref(reply))
        x.XFlush(d)
    elif e.type == 28 and e.prop.state == 1:
        key = (e.prop.window, e.prop.atom)
        if key in transfers:
            data, offset = transfers[key]
            size = 1 if mode == "unknown-tiny" else 65537 if mode.startswith("unknown") else 16384
            chunk = data[offset:offset + size]
            if not chunk and mode == "unknown-unterminated":
                del transfers[key]
                return
            x.XChangeProperty(d, key[0], key[1], utf8, 8, 0,
                              c.c_char_p(chunk), len(chunk))
            x.XFlush(d)
            if chunk:
                transfers[key] = (data, offset + len(chunk))
            else:
                del transfers[key]

def server_time():
    x.XChangeProperty(d, w, clock, 19, 8, 0, c.c_char_p(b""), 0)
    x.XFlush(d)
    while True:
        e = Event()
        x.XNextEvent(d, c.byref(e))
        if e.type == 28 and e.prop.window == w and e.prop.atom == clock:
            return e.prop.time
        event(e)

print("READY", flush=True)
try:
    while True:
        while x.XPending(d):
            e = Event()
            x.XNextEvent(d, c.byref(e))
            event(e)
        assert not errors, "unexpected X error"
        ready, _, _ = select.select([sys.stdin, x.XConnectionNumber(d)], [], [], 0.05)
        if sys.stdin not in ready:
            continue
        line = sys.stdin.readline(2 * 1024 * 1024 + 128)
        if not line:
            break
        command = line.strip().split()
        if command[0] in ("own", "same"):
            text = bytes.fromhex(command[1]) if command[1] != "-" else b""
            if command[0] == "own":
                acquired = server_time()
            assert acquired
            x.XSetSelectionOwner(d, clipboard, w, acquired)
            x.XSync(d, 0)
            assert x.XGetSelectionOwner(d, clipboard) == w
            print("OWNED", flush=True)
        elif command[0] == "stats":
            print(requests, flush=True)
        elif command[0] == "quit":
            break
        else:
            raise AssertionError("unknown test command")
finally:
    x.XCloseDisplay(d)
