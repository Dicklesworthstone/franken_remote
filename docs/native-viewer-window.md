# Client-owned native viewer window

This Linux/X11 slice connects the original viewer lifecycle to a locally chosen
presentation/input window. It is not a standalone desktop application or a
hardware/Wayland/physical-visibility qualification.

## Startup and ownership

Use `Viewer::observe_with_renderer` (or the equivalent on an already-approved
`ViewerSession`) when the remote display dimensions are not known before
startup. The renderer factory runs **once, after approval and explicit display
selection**. It receives the exact selected `Display` and the original
`StreamingViewerControl`. `None` for the clock policy is observation-only;
`Some(clock)` still requires the existing positive control negotiation. Neither
choice requests or grants input by itself.

Inside that factory, start `ViewerWindow` with the locally selected X display,
`Display::pixel_width`, `Display::pixel_height`, and the supplied stop handle.
Keep the `ViewerWindow` owner in the client shell, outside the factory future,
through streaming and decoder cleanup. Await `ViewerWindow::ready()` for a native
map/stop notification. Its single registered waker avoids adding a polling timer;
cancellation unregisters the waiter without losing the retained native owner.
Return `ViewerWindow::decoder_launch` with the locally installed worker image,
local Xauthority configuration and nonzero worker epoch. Package verification
remains installation policy, not a property of an absolute path.

The factory wait shares the original call-time bootstrap deadline. Existing
session control and observation renewal continue while native setup is pending.
Dropping an unpolled attempt, setup failure, or expiry terminates that same
session; it does not reconnect or create another grant. An already-polled
network turn is not abandoned merely because native setup completes.

Applications with an already chosen local window may instead retain
`Viewer::control()` before startup and use the existing `observe` entry point
with `Launch::present_in`. The old independent presenter mode is
unchanged; a selected-window startup cannot acknowledge that fallback.

## Pixels and input use the same local target

`WindowControl::target()` provides the locally created window and immutable
native-pixel dimensions. `WindowControl::input_window()` provides that same
window to the existing input capture adapter. The original controlled viewer
must still configure and confirm its real layout, prove the required view state,
and own the actual negotiated input lease and capture lifecycle. A decoder
reply never chooses the input window or supplies authority.

The private media pipe carries a bounded `ConfigurePresentation` request and
requires its exact `PresentationReady` echo. The worker validates the target
against the admitted codec dimensions. Its borrowed X11 presenter must not
create a substitute window, raise it, resize it, or destroy it. The client window
outlives normal borrowed-presenter destruction. A changed geometry, hide/remap,
or lost window permanently invalidates that presenter rather than resuming it.

This first window is exact-size, native-pixel presentation. There is no implicit
fit-to-window scaling, letterboxing, DPI conversion, or stretch-to-resize policy.
The X11 owner refuses sizes that do not fit the selected X screen. A future
scaler must coordinate the actual displayed rectangle with the original input
mapping; changing just one is not safe.

## Terminal behavior and cleanup

Closing the window, hiding it, changing its size, native failure, or dropping its
owner stops the original session before native cleanup. A stop handle retained
before decoder startup remains valid after observation and input promotion.
The promoted input owner also observes cancellation of that original context
immediately. Ordinary window moves do not change the pixel/input mapping.

`WindowControl::stop()` never waits for a codec, native call or network round
trip. A terminal status is **not** proof of remote key release or worker cleanup.
Retain each owner and collect its cleanup independently: `ViewerWindow::finish`
is nonblocking and idempotent; `Some` proves only its own native thread ended.
A blocked foreign X call can leave cleanup pending. The API does not hide that
limitation behind a timeout or claim that the blocked thread was killed.

The selected desktop user, X server and window manager remain trusted. Mapping,
X11 pixel readback and compositor submission are not proof of physical scanout,
user consent or a new input grant. No account, pairing mechanism, global input
grab, synthetic approval, authority extension or software-codec fallback is
introduced.

## Focused verification

With the repository-pinned Rust toolchain and the relevant development packages:

```sh
cargo test -p fr-media --test worker_protocol --locked
cargo test -p frd deferred_renderer --locked
cargo test -p fr-native --features linux-viewer-window --test viewer_window_x11 --locked
cargo test -p fr-native --features linux-media,linux-viewer-window --test viewer_window_x11 --locked
cargo test -p fr-native --features linux-media --test worker_process --locked
```

The window tests require Xvfb, X11/XCB libraries, Python 3 and OpenSSL; each test
creates a private X server. The combined media feature also requires the FFmpeg
SDK. These tests cover real pixels, resource ownership, terminal lifecycle,
original-context cancellation, exact startup framing and asynchronous display
setup. They do not qualify a live tailnet, hardware HEVC, a compositor, or the
complete desktop product.

## Composed desktop owner

With `fr-native`'s `linux-desktop` feature, `desktop::Desktop` retains the original
`NativeObserver`, native window, and cleanup handles as one single-use attempt.
Its `Configuration` takes local package and graphical-session settings, not
remote data. The local process must already have access to the selected X server;
passing an Xauthority path for the decoder does not change the process-wide
credentials used by the window or input threads.

`Desktop::open` takes an existing `Viewer`, the original observer/clock policies,
and approval/display-choice callbacks. It creates the exact-size window only
after that approved selection. An unpolled, expired or failed attempt cannot be
reused. Keep the `Desktop` after an error so a window already created by the
factory can still be collected. No new dialer, listener, transport qualification,
package trust decision or visibility witness is introduced by this API.

After startup, `serve` is observation-only. `serve_interactive` uses the original
viewing/requesting/controlled state machine: the UI requests control explicitly
and supplies the existing mapping and view evidence. Only after the original
grant may its UI callback return one confirmed `Layout` to start the existing
session-owned X11 input capture. The layout must exactly describe the full native
pixel image at `(0, 0)` in this window, including the selected display's signed
origin. A crop, toolbar offset, logical-DPI rectangle, scaled destination or
another display is refused, not silently translated. Subsequent callbacks return
`None`; they do not replace the input owner or restart a stopped capture. Input
receipts retain their original admission/submission/observation stages.

`configure_clipboard` connects the existing controller-only clipboard lifecycle
before service. Native access still waits for bilateral readiness and the
original control grant. Its returned control is the existing visible off switch;
`collect_clipboard` preserves bounded terminal results, including after close.

`close` fences the original session synchronously. `reap` does so at call time,
even when its future is never polled, and uses one original cleanup deadline.
Media, native input, window and clipboard have separate results; pending or
failed cleanup retains its owners rather than reporting remote held-key release
or physical pixel erasure. A native window loss during teardown invalidates the
borrowed presenter. Failed decoder bootstrap remains subject to the existing
worker startup cleanup and is not reported as a successful media reap.

Additional focused checks:

```sh
cargo test -p fr-native --features linux-desktop --test viewer_window_x11 --locked
cargo test -p fr-native --features linux-desktop input_mapping_must_describe --locked
```

The composed-owner tests use real XCB and TLS/UDP with a scripted peer that stops
at display selection. They exercise refusal, expiry, abandonment, UI unwinding,
window retention and separate cleanup outcomes. They do not establish a complete
native decoder/control/clipboard session, authenticated live-tailnet admission,
optical visibility, hardware media, or the desktop-shell phase gate. The full
native input suite remains a separate required check, not replaced by these
focused tests.
