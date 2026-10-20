# Native upload progress and local controls

An explicitly configured `fr connect --control --send PATH` selection now opens
an optional **Upload progress** utility window alongside its original Linux X11
viewer. It uses the original `SendControl` snapshot and supports files and
portable directory selections. Observation-only, receive-only and unconfigured
sessions do not open an upload panel.

The panel sets its non-input window hint before mapping and never requests
keyboard focus. Its **Pause**, **Resume** and **Cancel upload** buttons issue the
same local requests as the existing [physical shortcuts](NATIVE_FILE_SHORTCUTS.md).
Closing the panel only hides it: it does not cancel files, revoke control or close
the desktop. There is no automatic reopen or replacement selection. Shortcuts
and terminal receipts remain available when the panel is hidden or unavailable.

## Read the evidence, not just the bar

The bar represents **current-object bytes queued into the file channel**, not
bytes published at the recipient. A full bar can still be waiting for acceptance,
a publication receipt, or cleanup. Directory sizes remain unknown until the
original authorized source measures them. Zero-byte objects do not invent a
completion percentage; they still need a receipt.

The panel shows the transfer phase, current queued/total bytes, started selection
count, receipt count, publication counts by recipient, durability uncertainty,
publication uncertainty, refusal/interruption counts, and local source cleanup.
Names, paths, transfer IDs, peer text and file contents are never rendered.
Detailed typed refusal information and ordered per-selection receipts remain in
the terminal completion output; aggregate counts do not replace that evidence.

The latest local run/pause/cancel **request** is separate from the sender's last
**observed pause gate**. The observation can lag the request. A pause does not
interrupt already-issued reads or retract admitted bytes, and all original
handshake, record, transfer and batch deadlines continue running. Resume never
retries terminal work. Cancelling an upload is not proof of rollback: earlier
publication receipts and uncertain effects are retained. Local source cleanup
is separately reported and proves nothing about remote publication or erasure.

## Ownership and failure handling

`Desktop::configure_file_send` first obtains the original observer's successful
one-use upload configuration, then starts the optional panel on the confirmed
local viewer window. It reads no selected source and grants no permission. Both
interactive serving modes use that same configuration path. Panel creation
failure cannot turn successful file configuration into a retry or discard its
handle. `Desktop::file_progress()` exposes the panel's status/hide handle or a
local startup refusal; `None` means no successful upload configuration.

A private thread owns the panel's XCB connection. The decoder drawable, dimensions
and input mapping are unchanged; no video pixels pass through this UI. The panel
has fixed bounded text, a 64-event batch limit and a 100-ms snapshot refresh.
Unchanged snapshots do not redraw. Full-batch lifecycle/flood checks precede
commands; synthetic, disabled and unmatched gestures cannot issue requests.
Clicks queued across a stalled native turn are discarded, not retimed as fresh.
Displayed and current button availability are both checked before dispatch,
including races with shortcuts, cancellation and terminal receipts.

Original session termination stops the panel. Desktop closure fences the session
before stopping local UI. A hidden or failed panel is still retained until its
thread is joined: `window_cleanup()` stays pending while any panel cleanup is
pending, so reconnect cannot reuse an old UI owner. A stalled foreign X call does
not block the session authority thread, and a stop request is not represented
as completed native cleanup.

## Scope and validation

This is the existing XCB shell's fixed 640-by-310 utility surface, with the core
X11 fixed font, on a local 24-bit X11 screen large enough to contain it. A stale,
unmapped or resized parent is refused. The local X server and window manager
remain trusted. Window managers must respect the non-input utility-window hints;
other focus policies, accessibility, scaling/DPI and desktop-environment lanes
remain to be qualified. This is not a new cross-platform widget toolkit.

Sixteen repository tests cover the bounded model and production Rust/C input and
drawing paths, including six actual-X11 tests driven by independent Xlib/XTest
calls. They exercise button requests without focus transfer, progress-bar pixel
readback, terminal races, stale clicks, synthetic/invalid gestures, event floods,
parent loss and cleanup. The actual button/focus test also ran with Openbox's
distribution default configuration and the real viewer's fixed-size parent hints.
The source-check harness uses explicit session and transfer-result fixtures;
six additional fixture-backed checks execute the exact modified Desktop method
bodies for one-use binding, refused configuration, optional UI failure, original
session fencing and pending companion cleanup. These results do not qualify an
authenticated transfer, full current Desktop/CLI/Asupersync linking,
physical-user authenticity or a live tailnet.

Downloads, folder synchronization, cross-connection resumption, automatic file
opening, Wayland/macOS/Windows UI and a general accessibility implementation are
not added by this panel. Existing file and input authority checks are unchanged.
