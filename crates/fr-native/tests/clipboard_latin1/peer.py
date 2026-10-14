"""Independent Xlib text peer; fixture contents travel over stdin, not argv/logs."""
import json
import sys
from Xlib import X, Xatom, display, error, protocol

settings = json.loads(sys.stdin.readline())
d = display.Display(settings['display'])
root = d.screen().root
window = root.create_window(0, 0, 1, 1, 0, X.CopyFromParent, X.InputOutput,
                            X.CopyFromParent, event_mask=X.PropertyChangeMask)
clipboard = d.intern_atom('CLIPBOARD')
utf8 = d.intern_atom('UTF8_STRING')
timestamp = d.intern_atom('TIMESTAMP')
incr = d.intern_atom('INCR')
clock = d.intern_atom('_FR_LATIN1_TEST_CLOCK')
window.change_property(clock, Xatom.INTEGER, 8, b'x')
d.flush()
while True:
    event = d.next_event()
    if event.type == X.PropertyNotify and event.atom == clock:
        selected_at = event.time
        break
window.set_selection_owner(clipboard, selected_at)
d.sync()
assert d.get_selection_owner(clipboard).id == window.id
print('READY', flush=True)
body = bytes(settings.get('body', []))
pending = {}
while True:
    event = d.next_event()
    try:
        if event.type == X.SelectionRequest:
            requestor = event.requestor
            prop = event.property or event.target
            accepted = True
            if event.target == timestamp:
                requestor.change_property(prop, Xatom.INTEGER, 32, [selected_at])
            elif event.target in (utf8, Xatom.STRING):
                is_utf8 = event.target == utf8
                print('UTF8' if is_utf8 else 'STRING', flush=True)
                mode = settings.get('utf8_mode', 'refuse') if is_utf8 else settings.get('string_mode', 'data')
                if mode == 'silent':
                    continue
                if mode == 'refuse':
                    accepted = False
                else:
                    data = bytes(settings['utf8_body']) if is_utf8 and 'utf8_body' in settings else body
                    kind = event.target if mode != 'wrong_type' else Xatom.INTEGER
                    if settings.get('incremental'):
                        requestor.change_attributes(event_mask=X.PropertyChangeMask)
                        hint = settings.get('hint', len(data))
                        requestor.change_property(prop, incr, 32, [] if hint is None else [hint])
                        pending[(requestor.id, prop)] = (requestor, kind, data, 0)
                    else:
                        requestor.change_property(prop, kind, 8, data)
            else:
                accepted = False
            requestor.send_event(protocol.event.SelectionNotify(
                time=event.time, requestor=requestor, selection=clipboard,
                target=event.target, property=prop if accepted else X.NONE), propagate=False)
            d.flush()
        elif event.type == X.PropertyNotify and event.state == X.PropertyDelete:
            key = (event.window.id, event.atom)
            if key in pending:
                requestor, kind, data, offset = pending[key]
                end = min(offset + settings.get('chunk', 16385), len(data))
                requestor.change_property(event.atom, kind, 8, data[offset:end])
                if offset == end:
                    del pending[key]
                else:
                    pending[key] = (requestor, kind, data, end)
                d.flush()
    except error.BadWindow:
        # The requestor owns cancellation. Never redirect an abandoned transfer.
        pending.pop((event.window.id, event.atom), None)
