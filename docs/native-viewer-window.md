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
through streaming and decoder cleanup. Wait for `WindowControl::status()` to
become `Mapped` using asynchronous timers, not a blocking wait or a busy loop.
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
