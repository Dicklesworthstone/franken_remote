# Native file transfer progress and local controls

An explicitly configured `fr connect --control --send PATH`, `--file-drop` or `--receive DIR`
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
| Upload: `--send PATH` or `--file-drop` | Current-object bytes queued into the file channel; no percentage before selection | Pause, Resume, Cancel upload; Choose files when the original intake is ready |
| Download: `--receive DIR` | Current-object bytes privately staged in the local destination | Cancel download |

The panel sets its non-input window hint before mapping and never requests
keyboard focus. Upload Pause, Resume and Cancel issue the same local requests as the existing
[physical shortcuts](NATIVE_FILE_SHORTCUTS.md). Download cancellation uses the
original receive handle; it cannot pause, resume or restart a transfer.
Closing the panel hides progress and disables new selections while an already
accepted transfer keeps its own outcome. An open local chooser also has to close
and finish its bounded input return, as described below. There is no automatic
reopen or replacement selection. Upload
shortcuts and terminal receipts remain available when the panel is hidden or
unavailable.

## Choosing more files on Linux

An ordinary upload with the original session's `SendIntake` exposes **Choose
files...** at the upper right. It becomes available after the host's attachment
is ready and the preceding batch and source cleanup have finished. The same
button can make the first selection for `--file-drop`. A standing folder-upload
job owns its selected source and does not install this action.

Release held keys/buttons and finish any local text composition before choosing.
The original input producer must positively suspend before the desktop portal
opens; it forwards no dialog gestures. After the portal closes, the same input
owner must prove fresh focus, unchanged geometry and no held input before it
returns. This keeps the original control grant. A cancelled chooser submits no
files. If native closure or input return
cannot be confirmed within its original bound, the owner fences the connection
and retains the unfinished cleanup owner.

The last row reports chooser state independently of transfer receipts:

| Chooser message | Meaning |
| --- | --- |
| Choosing files locally | The original dialog owns the one selection slot; another click or drop is disabled. |
| Return to the remote desktop | The dialog has closed; focus and held-input checks are still pending. |
| File selection cancelled | No new batch was submitted; a later explicit selection can use the same live intake. |
| File selection unavailable | The local choice or original admission checks refused; no new batch was submitted. |
| File chooser cleanup unconfirmed | The original dialog could not prove closure; further native selections are disabled. |

The response's two-second freshness limit includes focus restoration, descriptor
selection and intake-lock waits. Expiry never renews a selection or replays it.
See [the connected upload workflow](NATIVE_FILE_SEND.md#additional-selections-during-a-native-desktop-session)
for portal requirements and the shared selection and byte bounds.

## Mac window controls

The `macos-desktop` viewer uses its reserved native sharing strip for both file
directions. The upload cell shows current queued/total bytes, a distinct pacing
label, **Pause / Resume**, and **Cancel**. Receive shows staged/total bytes and
its independent **Cancel**. The two-row upload cell fits inside the existing
230-by-44-point space; it does not change pointer mapping, cover the picture,
or require a larger fitted window.

The upload button follows initial selections, native drops/copied files and
local commands. It is bound to the exact batch whose state was displayed;
an older click never looks up the latest batch at dispatch. **Run pending** and
**Pause pending** mean that the latest request and the sender's observed gate
do not yet agree. **Running** and **Paused** reflect the sampled original gate,
not the peer's disk or the absence of already-admitted data. Fresh local clicks
are consumed only after a complete bounded native event batch; terminal native
events and original owner cancellation win over pending Resume.

The Mac strip samples at most ten times per second and contains no paths or
filenames. Terminal success still requires actual publication receipts and
source cleanup. See [pause/resume semantics](NATIVE_FILE_PACING.md#native-mac-upload-controls)
and [the Mac CLI](docs/native-client-cli.md#explicit-macos-file-transfer).
The following standalone-panel ownership and X11 qualification details apply
to the Linux utility windows. Mac AppKit layout and live transfer qualification
remain separate.

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

An upload configured with the original session's `SendIntake` can display later
explicit file choices or drops after successful publication and source cleanup. Each batch
keeps a separate `SendControl`. The same panel thread destroys its old native
window before opening a successor with that new immutable control, original
placement and fresh mapping evidence. It drains lifecycle events first, so a
queued close, unmap or parent loss suppresses replacement. Hidden or failed
panels never reopen. Hiding progress after a selection was accepted leaves that
transfer running and disables future native selections. Earlier completion receipts remain
in the CLI's bounded content-free history. See
[NATIVE_FILE_SEND.md](NATIVE_FILE_SEND.md#additional-selections-during-a-native-desktop-session).

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
The connected chooser change has the following executed evidence:

| Check | Result and scope |
| --- | --- |
| Actual portal and shared selection-inbox Rust sources | 39 tests passed with the pinned compiler and strict Clippy; portal cases use a private D-Bus broker and an explicit fixture service. |
| Original input-pause gate Rust source | 6 tests passed for original identity, refusal, deadlines, cancellation and positive return. |
| Actual native input C sources on authenticated Xvfb | 39 cases passed: 11 chooser-interval cases, 7 keyboard-map cases and 21 existing text/input cases. C compilation used `-Wall -Wextra -Werror`. |
| Actual progress-panel C ABI on authenticated Xvfb | One driver passed, checking visible chooser pixels, action 128, disabled/synthetic/unmatched gestures, stale press rejection and unchanged original focus. |

These are separate source and native-boundary checks. The authored full Rust
capture-loop/panel tests and authenticated ATP integration were not executed:
the pinned Asupersync dependency build was killed before compilation reached
these modules. A real desktop portal with a window manager, physical-user
authenticity and live-tailnet file sending remain unqualified by this evidence.
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
opening, Wayland/Windows UI and a general accessibility implementation are
not added by this panel. Original file grants, attachment identities and transfer
deadlines remain unchanged; ordinary focus loss still fences input outside the
explicitly suspended local chooser interval.
