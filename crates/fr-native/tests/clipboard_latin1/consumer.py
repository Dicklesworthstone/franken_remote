"""Independent native requestor. Assert actual properties; never print text."""
import json
import select
import sys
import time
from Xlib import X, Xatom, display

settings = json.loads(sys.stdin.readline())
d = display.Display(settings['display'])
w = d.screen().root.create_window(0, 0, 1, 1, 0, X.CopyFromParent,
    X.InputOutput, X.CopyFromParent, event_mask=X.PropertyChangeMask)
clipboard = d.intern_atom('CLIPBOARD')
targets = d.intern_atom('TARGETS')
utf8 = d.intern_atom('UTF8_STRING')
incr = d.intern_atom('INCR')
prop = d.intern_atom('_FR_LATIN1_TEST_RESULT')
print('READY', flush=True)

def event_until(deadline):
    while not d.pending_events():
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([d.fileno()], [], [], remaining)[0]:
            return None
    return d.next_event()

def raw(value):
    # Python-Xlib decodes any format-8 property as UTF-8 when possible,
    # irrespective of its native type. Undo that convenience before comparing
    # exact protocol bytes; this is not charset interpretation by the peer.
    return value.encode('utf-8') if isinstance(value, str) else bytes(value)

def property_value():
    # One bounded request avoids Python-Xlib's get_full_property concatenating
    # auto-decoded str and raw bytes across a split. No native reply may exceed
    # the actual owner's 16 KiB property bound.
    value = w.get_property(prop, X.AnyPropertyType, 0, 4096)
    assert value is not None and value.bytes_after == 0, 'unbounded or missing property'
    return value

def request(target):
    w.convert_selection(clipboard, target, prop, X.CurrentTime)
    d.flush()
    deadline = time.monotonic() + 4
    while True:
        event = event_until(deadline)
        assert event is not None, 'native request timed out'
        if event.type == X.SelectionNotify and event.target == target:
            assert event.requestor.id == w.id and event.selection == clipboard
            return event.property

if settings.get('pause_start'):
    assert sys.stdin.readline().strip() == 'GO'

assert request(targets) == prop
value = property_value()
assert value.property_type == Xatom.ATOM and value.format == 32
assert targets in value.value and utf8 in value.value
print('TARGETS_STRING' if Xatom.STRING in value.value else 'TARGETS_UTF8_ONLY', flush=True)
w.delete_property(prop)
target = utf8 if settings.get('target') == 'UTF8_STRING' else Xatom.STRING
result = request(target)
if settings.get('refused'):
    assert result == X.NONE, 'unrepresentable or excess request unexpectedly accepted'
    print('REFUSED', flush=True)
else:
    assert result == prop, 'text request refused'
    expected = bytes(settings['body'])
    value = property_value()
    if value.property_type == incr:
        assert settings.get('incremental', len(expected) > 16384)
        assert value.format == 32 and list(value.value) == [len(expected)], 'wrong native INCR size'
        print('INCR', flush=True)
        if settings.get('pause_header'):
            assert sys.stdin.readline().strip() == 'GO'
        w.delete_property(prop)
        d.flush()
        received = bytearray()
        chunks = 0
        deadline = time.monotonic() + (1 if settings.get('expect_timeout') else 4)
        while True:
            event = event_until(deadline)
            if event is None:
                assert settings.get('expect_timeout') and 0 < len(received) < len(expected)
                print('TIMED_OUT_PARTIAL', flush=True)
                break
            if event.type != X.PropertyNotify or event.state != X.PropertyNewValue or event.atom != prop:
                continue
            value = property_value()
            assert value.property_type == target and value.format == 8
            chunk = raw(value.value)
            assert len(chunk) <= 16384, 'unbounded native property'
            if not chunk:
                assert not settings.get('expect_timeout'), 'false success terminator'
                assert received == expected, 'native text bytes differ'
                print('MATCH', flush=True)
                break
            received.extend(chunk)
            assert len(received) <= len(expected), 'native text exceeds declaration'
            chunks += 1
            if chunks == settings.get('pause_after'):
                print('PAUSED', flush=True)
                assert sys.stdin.readline().strip() == 'GO'
                deadline = time.monotonic() + (1 if settings.get('expect_timeout') else 4)
            w.delete_property(prop)
            d.flush()
    else:
        assert not settings.get('incremental', len(expected) > 16384), 'missing INCR'
        assert value.property_type == target and value.format == 8
        assert raw(value.value) == expected, 'native text bytes differ'
        print('MATCH', flush=True)
d.close()
