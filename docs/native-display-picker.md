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
