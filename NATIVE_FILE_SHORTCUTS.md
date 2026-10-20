# Native upload controls from the focused viewer

For an explicitly configured `fr connect --control --send PATH` selection, the
original X11 input capture recognizes these **physical** shortcuts:

| Shortcut | Local request |
|---|---|
| Ctrl+Alt+Shift+F8 | Pause the current upload selection |
| Ctrl+Alt+Shift+F9 | Resume that same selection on that same connection |
| Ctrl+Alt+Shift+F10 | Cancel that upload selection, not the desktop |

Either side of each modifier works. Press the modifiers first, then the function
key; release the function key between commands. The action key's press, repeats
and release never reach the host. Modifier presses/releases retain their normal
remote meaning. A function key pressed before its modifiers remains a remote
key for that whole gesture, including its release. Autorepeat never creates a
second command. `Ctrl+Alt+Shift+Escape` remains the emergency local stop and has
priority over **all** commands in its triggering native batch.

These bindings are enabled only after the original desktop configures an upload
and attaches native input to its confirmed local window/layout. No upload means
no reserved shortcuts. Observation-only sessions and downloads get no new input
or file authority. Native focus loss still ends control; there is no terminal
input reader, global keyboard hook, input grab, or reconnect-time replay.

## Requests are not receipts

The commands call the existing original `SendControl::pause`, `resume`, and
`cancel`. They do not create another transfer, extend its deadline, change byte
budgets, grant permission, or retract bytes already issued. A pause during setup
keeps the original handshake deadline. Cancellation remains terminal and the
existing owner continues cleanup; publication and uncertainty receipts remain
separate. A stale handle cannot restart finished, cancelled, or expired work.
The handle's `SendSnapshot` still distinguishes requested pacing from the last
observed sender gate. No remote-idle, rollback, or cleanup-success claim follows
from pressing a key. This change adds shortcuts, not a new progress dialog.

## Native input boundaries

Physical XKB names, not keysyms, fixed keycodes or text, identify the shortcuts.
Missing or ambiguous modifier/function-key names disable this optional mapping;
they never weaken the mandatory emergency escape. `CaptureControl` exposes
`file_shortcuts_available()` as mapping evidence (unknown during initialization,
false if unavailable). A binding is one-use: a retained capture cannot be
redirected to another selection. A new desktop starts unbound.

The original native batch checks (focus, geometry, device/keymap changes,
synthetic events, event-count limits) precede command dispatch. Original event
timestamps remain checked; queued old shortcuts cannot acquire a new timestamp
at dequeue. Local-only held gestures use the existing bounded release-only
sampler even without negotiated keyboard forwarding. Native snapshots can only
clear locally observed presses, never invent a gesture or command.

## Evidence and remaining scope

Tests cover physical modifier-side/order combinations, gestures spanning
batches, autorepeat, enabling during a remote key press, release ordering,
release-only snapshots, missing/ambiguous maps, and unconfigured forwarding.
Actual-X11 tests execute the production Rust input loop and C boundary with an
independent Xlib/XTest driver: commands remain local, ordinary input continues,
and stale or invalid batches and emergency escape cannot dispatch file commands.
The test Target supplies explicit source authority and records local requests;
it is not a live authenticated transfer or physical-user authenticity proof.

Full desktop/transport/codec linking and live-tailnet qualification remain
separate. The existing X server and selected local user remain trusted. No
Wayland, macOS, Windows, touch, graphical transfer buttons, download pacing or
cross-connection transfer resumption is implemented by this change.
