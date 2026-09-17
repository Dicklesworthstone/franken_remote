#!/usr/bin/env python3
"""Independent Xlib/XTest test window and commands. Never shipping code."""
import ctypes as c
import sys
import time
x = c.CDLL('libX11.so.6'); xt = c.CDLL('libXtst.so.6')
D, W = c.c_void_p, c.c_ulong
def sig(lib, n, r, *args):
    f = getattr(lib,n); f.restype=r; f.argtypes=list(args); return f
sig(x,'XOpenDisplay',D,c.c_char_p); sig(x,'XCloseDisplay',c.c_int,D)
sig(x,'XDefaultRootWindow',W,D); sig(x,'XDefaultScreen',c.c_int,D)
sig(x,'XCreateSimpleWindow',W,D,W,c.c_int,c.c_int,c.c_uint,c.c_uint,c.c_uint,W,W)
sig(x,'XMapWindow',c.c_int,D,W); sig(x,'XSetInputFocus',c.c_int,D,W,c.c_int,W)
sig(x,'XSync',c.c_int,D,c.c_int); sig(x,'XUnmapWindow',c.c_int,D,W)
sig(x,'XResizeWindow',c.c_int,D,W,c.c_uint,c.c_uint)
sig(x,'XDestroyWindow',c.c_int,D,W)
sig(x,'XChangeKeyboardMapping',c.c_int,D,c.c_int,c.c_int,c.POINTER(W),c.c_int)
sig(x,'XGetKeyboardMapping',c.POINTER(W),D,c.c_ubyte,c.c_int,c.POINTER(c.c_int))
sig(x,'XFree',c.c_int,D)
sig(x,'XSendEvent',c.c_int,D,W,c.c_int,c.c_long,D)
sig(xt,'XTestFakeKeyEvent',c.c_int,D,c.c_uint,c.c_int,W)
sig(xt,'XTestFakeMotionEvent',c.c_int,D,c.c_int,c.c_int,c.c_int,W)
sig(xt,'XTestFakeButtonEvent',c.c_int,D,c.c_uint,c.c_int,W)
class Key(c.Structure):
    _fields_=[('type',c.c_int),('serial',W),('send_event',c.c_int),('display',D),('window',W),('root',W),('subwindow',W),('time',W),('x',c.c_int),('y',c.c_int),('x_root',c.c_int),('y_root',c.c_int),('state',c.c_uint),('keycode',c.c_uint),('same_screen',c.c_int)]
d=x.XOpenDisplay(sys.argv[1].encode('ascii')); assert d
root=x.XDefaultRootWindow(d)
w=x.XCreateSimpleWindow(d,root,0,0,320,240,0,0,0xffffff); assert w
x.XMapWindow(d,w); x.XSetInputFocus(d,w,2,0); x.XSync(d,0)
print(w,flush=True)
try:
 for command in sys.stdin:
    parts=command.split(); op=parts[0]
    if op=='key':
        xt.XTestFakeKeyEvent(d,int(parts[1]),int(parts[2]),0)
    elif op=='motion':
        xt.XTestFakeMotionEvent(d,x.XDefaultScreen(d),int(parts[1]),int(parts[2]),0)
    elif op=='button':
        xt.XTestFakeButtonEvent(d,int(parts[1]),int(parts[2]),0)
    elif op=='focus': x.XSetInputFocus(d,root,2,0)
    elif op=='restore': x.XSetInputFocus(d,w,2,0)
    elif op=='unmap': x.XUnmapWindow(d,w)
    elif op=='destroy': x.XDestroyWindow(d,w)
    elif op=='resize': x.XResizeWindow(d,w,160,120)
    elif op=='remap':
        count=c.c_int(); old=x.XGetKeyboardMapping(d,38,1,c.byref(count))
        assert old and count.value>0
        # Change only logical symbols, not the physical XKB name; even this
        # invalidates the armed capture generation.
        x.XChangeKeyboardMapping(d,38,count.value,old,1); x.XFree(old)
    elif op=='synthetic':
        event=Key(2,0,0,d,w,root,0,0,10,10,10,10,0,38,1)
        buf=c.create_string_buffer(192); c.memmove(buf,c.byref(event),c.sizeof(event))
        x.XSendEvent(d,w,False,1,buf)
    elif op=='flood':
        for i in range(100): xt.XTestFakeMotionEvent(d,x.XDefaultScreen(d),i+1,10,0)
    elif op=='quit': break
    else: raise RuntimeError('unknown test operation')
    x.XSync(d,0); print('ok',flush=True)
finally: x.XCloseDisplay(d)
