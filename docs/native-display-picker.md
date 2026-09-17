# In-session native display selection

`fr_native::display_picker::DisplayPicker` implements a finite X11 display-choice
surface using the existing XCB viewer-window boundary. It accepts an already
approved, bounded `Catalog` and the original `StreamingViewerControl`. The
containing viewer must continue its original startup, approval, renewal and
absolute-deadline service while this native UI waits. A picker is not admission,
visibility evidence, input permission or a replacement session.

The window shows each catalog entry's pixel dimensions and signed desktop
origin. No thumbnails, host-provided text, paths or persistent monitor IDs are
invented. The selected local X server and desktop user remain trusted. There is
no global input grab or injection and no new runtime, GUI dependency or toolkit.
This first X11 surface uses a fixed bitmap font and bounded geometry; it is not a
claim of accessibility/DPI, Wayland, hardware-surface or physical-scanout qualification.

The user can click a row, navigate with arrows then Enter, or use a number key.
No row starts selected, and an unmatched release or cross-row drag cannot choose.
Synthetic input events cannot select. Escape, window-manager close, hide,
resize, loss, native failure and bounded-event overflow stop the original
attempt. Native work is confined to one thread and is not on the authority path.

`poll(&catalog)` returns one full-width session-local handle only after the
native thread has closed its window and has been joined. It rejects changes to
revision, order or any display metadata rather than translating a stale choice.
It must be used in the SAME original selection exchange, never to manufacture
monitor continuity across reconnects. Old consumed picker handles are retired;
they cannot stop the subsequent viewer or choose twice.

Stop and cleanup remain separate: cancel fences the original session without
waiting for native calls, and `finish()` nonblockingly reports whether the
native thread has actually ended. A blocked native call can leave it pending.
Keep the picker owner through failed startup and collect it before replacement.
Dropping an outstanding owner cancels; dropping a consumed owner does not cancel
the viewing session to which selection was handed off.

Focused tests on a provisioned builder:

```sh
cargo test -p fr-native --features linux-viewer-window --test viewer_window_x11 picker:: --locked -- --test-threads=1
cargo clippy -p fr-native --features linux-desktop --all-targets --locked -- -D warnings
```

The tests use private real Xvfb servers, XCB rendering, independent Xlib pixel
readback and XTest gestures, with the original localhost TLS/UDP viewer lifetime.
They are native UI/ownership evidence, not installed-Tailscale admission, host
approval, complete FFmpeg desktop or hardware/physical-presentation qualification.

## Original desktop and reconnect ownership

`desktop::Configuration::with_display_picker()` explicitly uses the chooser
instead of the caller's synchronous choice callback. `Desktop::open` constructs
it only from the current approved catalog and the original stop handle; renewal
and all startup deadlines continue through the existing session service. The
renderer factory is not entered until the picker has returned its exact alias.

`Desktop::picker()` exposes only its content-free status/cancellation handle.
`picker_cleanup()` and `Cleanup::picker` distinguish NotStarted, Pending and
Complete. The reconnect `Session` preserves only the choice policy, never a
picker/catalog/result, and waits for picker cleanup alongside the original
media, input, window and clipboard owners before another attempt.

A cancelled chooser may finish before a decoder ever exists. `Session` records
that real cancellation before cleanup and combines the one-use renderer-factory
entry marker with every resource's cleanup receipt. Only completed picker cleanup (or a picker never started),
no renderer entry and no media/input/window/clipboard owner can complete a
pre-render failure or cancellation path. A missing worker handle alone is not such proof;
incomplete decoder bootstraps after renderer entry remain terminal and retained.

The composed tests include a multi-monitor wire selection, pending approval with
no native UI, timeout-vs-user-cancellation, an expired cleanup wait followed by
collection of the same owner, and two real XCB/localhost-TLS viewing lifetimes
with retired picker handles. The media children in the last case are the
repository's explicit recorded-HEVC fixtures, not real FFmpeg execution.
