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
sig(x,'XKeysymToKeycode',c.c_ubyte,D,W)
sig(x,'XGrabKey',c.c_int,D,c.c_int,c.c_uint,W,c.c_int,c.c_int,c.c_int)
sig(x,'XGrabKeyboard',c.c_int,D,W,c.c_int,c.c_int,c.c_int,W)
sig(x,'XUngrabKeyboard',c.c_int,D,W)
sig(x,'XPending',c.c_int,D); sig(x,'XNextEvent',c.c_int,D,D)
sig(x,'XFetchName',c.c_int,D,W,c.POINTER(D))
sig(xt,'XTestFakeKeyEvent',c.c_int,D,c.c_uint,c.c_int,W)
sig(xt,'XTestFakeMotionEvent',c.c_int,D,c.c_int,c.c_int,c.c_int,W)
sig(xt,'XTestFakeButtonEvent',c.c_int,D,c.c_uint,c.c_int,W)
class Key(c.Structure):
    _fields_=[('type',c.c_int),('serial',W),('send_event',c.c_int),('display',D),('window',W),('root',W),('subwindow',W),('time',W),('x',c.c_int),('y',c.c_int),('x_root',c.c_int),('y_root',c.c_int),('state',c.c_uint),('keycode',c.c_uint),('same_screen',c.c_int)]
class Focus(c.Structure):
    _fields_=[('type',c.c_int),('serial',W),('send_event',c.c_int),('display',D),('window',W),('mode',c.c_int),('detail',c.c_int)]
class Event(c.Union):
    _fields_=[('type',c.c_int),('key',Key),('pad',c.c_long*24)]
class Modifiers(c.Structure):
    _fields_=[('max_keypermod',c.c_int),('modifiermap',c.POINTER(c.c_ubyte))]
sig(x,'XGetModifierMapping',c.POINTER(Modifiers),D)
sig(x,'XFreeModifiermap',c.c_int,c.POINTER(Modifiers))
# XI hierarchy changes are confined to the explicitly requested test X server.
xi=c.CDLL('libXi.so.6')
class Add(c.Structure):
    _fields_=[('type',c.c_int),('name',c.c_char_p),('send_core',c.c_int),('enable',c.c_int)]
class Remove(c.Structure):
    _fields_=[('type',c.c_int),('deviceid',c.c_int),('return_mode',c.c_int),('return_pointer',c.c_int),('return_keyboard',c.c_int)]
class Device(c.Structure):
    _fields_=[('deviceid',c.c_int),('name',c.c_char_p),('use',c.c_int),('attachment',c.c_int),('enabled',c.c_int),('num_classes',c.c_int),('classes',D)]
sig(xi,'XIQueryVersion',c.c_int,D,c.POINTER(c.c_int),c.POINTER(c.c_int))
sig(xi,'XIChangeHierarchy',c.c_int,D,D,c.c_int)
sig(xi,'XIQueryDevice',c.POINTER(Device),D,c.c_int,c.POINTER(c.c_int))
sig(xi,'XIFreeDeviceInfo',None,c.POINTER(Device))
masters=[]
def add_master():
    major=c.c_int(2);minor=c.c_int(0)
    assert xi.XIQueryVersion(d,c.byref(major),c.byref(minor))==0
    add=Add(1,b'fr-test',True,True)
    assert xi.XIChangeHierarchy(d,c.byref(add),1)==0
    count=c.c_int(); devices=xi.XIQueryDevice(d,1,c.byref(count)); assert devices
    try:
        for i in range(count.value):
            dev=devices[i]
            if dev.use==1 and dev.name==b'fr-test pointer': masters.append(dev.deviceid)
    finally: xi.XIFreeDeviceInfo(devices)
    assert masters

d=x.XOpenDisplay(sys.argv[1].encode('ascii')); assert d
root=x.XDefaultRootWindow(d)
w=x.XCreateSimpleWindow(d,root,0,0,320,240,0,0,0xffffff); assert w
x.XMapWindow(d,w); x.XSetInputFocus(d,w,2,0); x.XSync(d,0)
codes=[int(x.XKeysymToKeycode(d,key)) for key in [0xffe9,0xff09,0xffeb]]
assert all(codes)
pressed_keys={}; pressed_buttons={}
def key(code,down):
    assert xt.XTestFakeKeyEvent(d,code,down,0)
    if down: pressed_keys[code]=True
    else: pressed_keys.pop(code,None)
def button(code,down):
    assert xt.XTestFakeButtonEvent(d,code,down,0)
    if down: pressed_buttons[code]=True
    else: pressed_buttons.pop(code,None)
def release():
    for code in reversed(list(pressed_keys)): key(code,0)
    for code in reversed(list(pressed_buttons)): button(code,0)
def local_keys():
    # No ordinary root event selection: only the fixture's passive grabs can
    # deliver these keys. Production capture uses a different XCB connection.
    x.XSync(d,0); result=[]; consumed=0
    while x.XPending(d):
        event=Event(); x.XNextEvent(d,c.byref(event)); consumed+=1
        assert consumed<=128
        if event.type in (2,3) and event.key.window==root:
            assert not event.key.send_event
            result.append([int(event.key.keycode),int(event.type)])
    return result
def passive_shortcuts():
    mapping=x.XGetModifierMapping(d); assert mapping
    try:
        n=mapping.contents.max_keypermod; assert 0<n<=32
        mask=0
        for modifier in range(8):
            if any(mapping.contents.modifiermap[modifier*n+i]==codes[0] for i in range(n)):
                mask|=1<<modifier
        assert mask and not mask&(mask-1)
    finally: x.XFreeModifiermap(mapping)
    # Both modes are asynchronous; this fixture cannot freeze the X server.
    x.XGrabKey(d,codes[1],mask,root,False,1,1)
    x.XGrabKey(d,codes[2],0,root,False,1,1)
print(w,flush=True)
try:
 for command in sys.stdin:
    parts=command.split(); op=parts[0]; response='ok'
    if op=='key':
        key(int(parts[1]),int(parts[2]))
    elif op=='motion':
        xt.XTestFakeMotionEvent(d,x.XDefaultScreen(d),int(parts[1]),int(parts[2]),0)
    elif op=='button':
        button(int(parts[1]),int(parts[2]))
    elif op=='release': release()
    elif op=='shortcut_codes': response=' '.join(str(code) for code in codes)
    elif op=='passive_shortcuts': passive_shortcuts()
    elif op=='shortcut':
        if parts[1]=='alt_tab':
            for code,down in [(codes[0],1),(codes[1],1),(codes[1],0),(codes[0],0)]: key(code,down)
        elif parts[1]=='super': key(codes[2],1); key(codes[2],0)
        else: raise RuntimeError('unknown shortcut')
    elif op=='local_keys': response=' '.join(f'{code}:{kind}' for code,kind in local_keys())
    elif op=='grab': response=str(x.XGrabKeyboard(d,w,False,1,1,0))
    elif op=='ungrab': x.XUngrabKeyboard(d,0)
    elif op=='probe_grab':
        # A separate client never steals an existing grab. Release immediately
        # after success, so the test observes availability without retaining it.
        observer=x.XOpenDisplay(sys.argv[1].encode('ascii')); assert observer
        try:
            result=x.XGrabKeyboard(observer,w,False,1,1,0)
            if result==0: x.XUngrabKeyboard(observer,0)
            x.XSync(observer,0); response=str(result)
        finally: x.XCloseDisplay(observer)
    elif op=='title':
        title=D()
        if x.XFetchName(d,w,c.byref(title)) and title:
            try: response=c.string_at(title).decode('utf-8')
            finally: x.XFree(title)
        else: response='<none>'
    elif op=='focus': x.XSetInputFocus(d,root,2,0)
    elif op=='restore': x.XSetInputFocus(d,w,2,0)
    elif op=='map': x.XMapWindow(d,w); x.XSetInputFocus(d,w,2,0)
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
    elif op=='synthetic_focus':
        # Match the acquisition notification's mode/detail after readiness;
        # SendEvent must remain a negative fence, never an activation allowance.
        event=Focus(10,0,0,d,w,1,3) # FocusOut, NotifyGrab, NotifyNonlinear
        buf=c.create_string_buffer(192); c.memmove(buf,c.byref(event),c.sizeof(event))
        x.XSendEvent(d,w,False,1<<21,buf)
    elif op=='flood':
        for i in range(100): xt.XTestFakeMotionEvent(d,x.XDefaultScreen(d),i+1,10,0)
    elif op=='hierarchy': add_master()
    elif op=='quit': break
    else: raise RuntimeError('unknown test operation')
    x.XSync(d,0); print(response,flush=True)
finally:
 release()
 x.XUngrabKeyboard(d,0)
 for device in masters:
    remove=Remove(2,device,2,0,0) # return attached slaves floating; no ID assumptions
    xi.XIChangeHierarchy(d,c.byref(remove),1)
 x.XSync(d,0)
 x.XCloseDisplay(d)
