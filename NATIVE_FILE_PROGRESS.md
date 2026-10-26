# Native file transfer progress and local controls

An explicitly configured `fr connect --control --send PATH` or `--receive DIR`
opens an optional progress panel alongside its original Linux X11 viewer. The
panel reads the original transfer owner's bounded status. Files and portable
directory selections use the same panel; observation-only and unconfigured
sessions do not open one.

With the negotiated duplex file capability, `--send PATH --receive DIR` keeps
both directions active on the same controlling connection. Each direction has
its own progress panel, original cancellation handle and retained native owner.
Opening or hiding one panel never replaces the other direction's owner.

| Direction | Progress bar | Local controls |
| --- | --- | --- |
| Upload: `--send PATH` | Current-object bytes queued into the file channel | Pause, Resume, Cancel upload |
| Download: `--receive DIR` | Current-object bytes privately staged in the local destination | Cancel download |

The panel sets its non-input window hint before mapping and never requests
keyboard focus. Upload buttons issue the same local requests as the existing
[physical shortcuts](NATIVE_FILE_SHORTCUTS.md). Download cancellation uses the
original receive handle; it cannot pause, resume or restart a transfer.
Closing the panel only hides it: it does not cancel files, revoke control or close
the desktop. There is no automatic reopen or replacement selection. Upload
shortcuts and terminal receipts remain available when the panel is hidden or
unavailable.

## Progress and publication

Neither progress bar establishes publication. A full bar can still be waiting
for integrity verification, an atomic publication, its receipt, or cleanup.
Directory sizes remain unknown until the original authorized source measures
them. Zero-byte objects do not invent a completion percentage; they still need
a receipt.

The upload panel shows the transfer phase, current queued/total bytes, started
selection count, receipt count, publication counts by recipient, durability
uncertainty, publication uncertainty, refusal/interruption counts, and local
source cleanup. The download panel shows the receive phase, current staged/total
bytes, actual publication and refusal counts, uncertain outcomes, whether the
bounded result history was truncated, cancellation intent, and local disk cleanup.
Names, paths, transfer IDs, peer text and file contents are never rendered.
Detailed typed refusal information and ordered per-selection receipts remain in
the terminal completion output; aggregate counts do not replace that evidence.
A missing or truncated result cannot establish that an item had no effect.

For uploads, the latest local run/pause/cancel **request** is separate from the sender's last
**observed pause gate**. The observation can lag the request. A pause does not
interrupt already-issued reads or retract admitted bytes, and all original
handshake, record, transfer and batch deadlines continue running. Resume never
retries terminal work. Cancelling an upload is not proof of rollback: earlier
publication receipts and uncertain effects are retained. Local source cleanup
is separately reported and proves nothing about remote publication or erasure.
Download cancellation likewise preserves earlier local publications. Its cleanup
status reports the original disk owner's completion, independently of its
publication receipts. Cancellation follows the existing transport custody rules;
it does not discard admitted native bytes or extend a deadline.

## Ownership and failure handling

`Desktop::configure_file_send` and `Desktop::request_receive_files` first obtain
the original observer's successful one-use configuration, then start the matching
optional panel on the confirmed local viewer window. The panel receives no source
or destination descriptor and grants no permission. Both interactive serving
modes use these configuration paths. Panel creation failure cannot turn
successful file configuration into a retry or discard its handle.
`Desktop::upload_progress()` and `Desktop::receive_progress()` expose the exact
direction's status/hide handle or local startup refusal; `None` means that
direction has no successful configuration. Duplex callers use these explicit
accessors. The existing `Desktop::file_progress()` remains compatible with
single-direction callers: it returns upload when configured, otherwise download.
It does not combine the directions' state or select a replacement owner.

A private thread owns the panel's XCB connection. The decoder drawable, dimensions
and input mapping are unchanged; no video pixels pass through this UI. A panel
needs an available rectangle beside the original viewer. If no such rectangle
fits, the optional panel is refused while the accepted transfer continues.
When the Desktop has clipboard layout intent or has configured its clipboard,
file panels reserve the clipboard's unchanged preferred rectangle. The second
file panel also reserves the first file panel's original placement. Both
configuration orders use this policy, including retaining the first panel's
actual layout policy if clipboard is configured between the file directions.
The bounded placement search never moves the viewer, moves the clipboard panel,
or changes the decoder/input mapping. Real mapped file-panel frames are checked
against the viewer, screen bounds and their original reserved rectangles before
accepting commands and on refresh; the clipboard retains its independent actual
unobscured-visibility requirement and watchdog. A window manager that ignores
placement hints may still make an optional panel unusable.
Reservations describe the initial positions; they do not track or automatically
retile sibling panels subsequently moved by an operator or window manager.
Viewer visibility and input fences remain active. The panel
has fixed bounded text, a 64-event batch limit and a 100-ms snapshot refresh.
Unchanged snapshots do not redraw. Full-batch lifecycle/flood checks precede
commands; synthetic, disabled and unmatched gestures cannot issue requests.
Clicks queued across a stalled native turn are discarded, not retimed as fresh.
Displayed and current button availability are both checked before dispatch,
including races with shortcuts, cancellation and terminal receipts.

Original session termination stops both panels. Desktop closure fences the session
before stopping local UI. A hidden or failed panel is still retained until its
thread is joined: `window_cleanup()` stays pending while any panel cleanup is
pending, so reconnect cannot reuse an old UI owner. A stalled foreign X call does
not block the session authority thread, and a stop request is not represented
as completed native cleanup.

## Scope and validation

This is the existing XCB shell's fixed 640-by-310 utility surface, with the core
X11 fixed font, on a local 24-bit X11 screen with room beside the viewer. A stale,
unmapped or resized parent is refused. The local X server and window manager
remain trusted. Window managers must respect the non-input utility-window hints;
other focus policies, accessibility, scaling/DPI and desktop-environment lanes
remain to be qualified. This is not a new cross-platform widget toolkit.

Repository targets cover the bounded upload/download models and production
Rust/C input and drawing paths, including independent Xlib/XTest peers. Cases
include button requests without focus transfer, staged-byte pixel readback,
publication and cleanup uncertainty, terminal races, stale clicks,
synthetic/invalid gestures, event floods, parent loss and cleanup. Their session
and transfer-result fixtures are explicit.
Concurrent-panel regressions exercise either file configuration order, distinct
native button routing, preserved clipboard visibility and off control, hiding
one direction while the other remains usable, and both native cleanup barriers.

The native download CLI target in
`crates/frd/tests/native_host_linux_serial/real_downloads/progress.rs` also checks
the panel's original-parent binding, nonfocus hints, placement and hide behavior
while actual host-selected files publish and input remains active on the original
session. It uses the existing real UDP/TLS/ATP and disk path with synthetic
admission and Xvfb displays. These targets do not establish physical-user
authenticity, general window-manager compatibility or a live tailnet.

Folder synchronization, cross-connection resumption, automatic file
opening, Wayland/macOS/Windows UI and a general accessibility implementation are
not added by this panel. Existing file and input authority checks are unchanged.
