# Native X11 viewer input

`fr-native` feature `linux-viewer-input` supplies `viewer_input::X11InputCapture`.
It connects logical input on a local renderer-owned X11 window to the **original
controlled viewer's existing bounded event queue**. It does not create a window,
start a session, obtain permission, confirm presentation, or acquire control.

After the application's renderer has applied and confirmed its `Layout`, consume
`ControlledViewer::capture_input()` and pass that source, the same layout token,
an explicit local `:N` / `:N.S` display, and the renderer's XID and pixel size to
`X11InputCapture::start`. Continue the usual controlled-session driver. No second
input sequence owner, transport path, or async runtime is installed. A layout from
another viewport cannot become valid merely because its numbers match.

```rust,ignore
// `applied_layout` must already be applied by the renderer and confirmed on viewer.
let source = viewer.capture_input()?;
let capture = X11InputCapture::start(local_display, local_window, source, applied_layout)?;
let local_stop = capture.control();
// Retain capture, continue driving viewer; collect capture.finish() during cleanup.
```

Initialization requires a mapped, focused, non-root window of the supplied size,
XKB key names and per-client detectable autorepeat, with no initially held keys
or core buttons. It never selects input on a root, grabs input, changes focus,
injects events, or changes server-wide autorepeat. Only physical keys with known
XKB names are mapped to the shared HID table; symbols are not guessed from layout.
Pointer motion, primary/middle/secondary/back/forward buttons, and discrete wheel
steps use the original layout identity. Missing granted capabilities are not
emulated. Repeat presses are sent only when Repeat was granted; otherwise native
repeat presses are suppressed. IME, committed text, pixel scrolling, relative
pointer lock, secure attention sequences and global hotkeys are not provided by
this adapter. Text must use a separately qualified text-input path, not keysyms or
clipboard substitution.

The native pump has its own XCB connection and thread, separate from the decoder
and network driver. It stages at most 64 events (ignored events count), including
one batch's lifecycle events before admitting any input. Focus loss, hiding,
resizing, keymap changes, synthetic SendEvent input, native failure and overflow
stop the original source. A focus gain cannot resume that grant. Application
suspend, renderer invalidation and local disconnect must also call the retained
stop handle. It cancels the original viewer synchronously without waiting for an
X call; remotely held key/button cleanup remains with the host input agent.

A private server-clock property bounds each active batch. Its timestamp and the
client time sampled **before sending the barrier** provide a conservative event
time, including millisecond quantization. Event age is not reset on dequeue or
backpressure. Regressing/future native timestamps, expired records and stalled
barriers refuse; u32 X timestamp wrap is supported. The existing 100-ms input
queue deadline, all host authorization deadlines and viewport checks remain in
force. After initial arming, idle polling sends no native requests or properties.

`CaptureControl::status` contains no input values. `stop` and Drop fence input,
but do not claim foreign-call cleanup has finished. Retain `X11InputCapture`
until its idempotent `finish()` returns Some to prove that this original thread
has ended and its X resources have been released. A blocked setup call cannot be
killed by dropping an async future.

## Evidence and limits

The native unit target uses a real Xvfb server with an independent Xlib/XTest peer.
It exercises keys, pointer/buttons/wheel, focus/unmap/resize/keymap changes,
synthetic-input refusal, initial held state, stale backlog and event floods. The
native tests' recording target, clock and initial layout are explicit fixtures,
not a claim of native-window-to-live-tailnet qualification. Daemon event tests
separately exercise the actual original Source, UDP/TLS, ordered dispatch,
backpressure, capability selection and stop propagation. No production test grant
or test-only Source constructor is introduced.

The selected user, X server and input stack are trusted. XTest cannot establish
physical human intent, and a mapped window is not proof of compositor visibility.
This adapter still requires a native shell to supply its actual renderer window,
applied layout and lifecycle callbacks. It does not export a decoder worker's
window or pretend that a fixture renderer is that native shell.

```sh
FR_NATIVE_INPUT_CAPTURE_REQUIRED=1 xvfb-run -a \
  -s '-screen 0 1280x1024x24 -noreset -nolisten tcp' \
  cargo test -p fr-native --features linux-viewer-input --lib --locked
```

Advances `fr-p1-desktop-shell-1-1sq` and `fr-p1-fr-client-bis`; neither is closed.
