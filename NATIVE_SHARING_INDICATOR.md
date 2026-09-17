# Native X11 sharing indicator

`fr-native` feature `linux-session-ui` exposes `sharing_indicator::SharingIndicator`.
It renders a real local X11 window with a Stop Sharing button and Escape, Enter,
and Space accelerators. The label reports that sharing is authorized, not that
pixels have been captured or presented. There are no peer-provided strings.

Pass an explicit local display (`:N` or `:N.S`) and the original
`HostSession::observation()` handle to `SharingIndicator::start`. Retain the
indicator while that original session is shared. Check its `control().status()`
and wait for `Mapped` before starting capture; this boundary does not itself
start or gate the application's publisher. `Mapped` means a server-authored map
notification arrived and drawing requests were submitted, not that a human saw
pixels. Native initialization, rendering and event processing use a dedicated
thread and a separately owned XCB connection, never a network or media callback.
The server must supply the core `6x13` font. No shell command or font downloader
is used by the production implementation.

Button activation, the accelerators, window-manager close, unmapping, obscuring,
resizing, native failure, authority expiry, owner drop, and the independent
`IndicatorControl::stop` all revoke the ORIGINAL observation owner. That closes
the same shared authority consulted before input and media submission. It never
looks up another session by numeric ID. The control is revocation-only: synthetic
local events can remove authority but cannot establish map evidence, approve a
session, re-enable clipboard, or grant control. Keyboard-map changes refresh only
the stop accelerators. No keyboard/pointer grab or global hotkey is installed.

Each native turn handles at most 32 events, checks authority between events,
coalesces drawing, and sleeps 10 ms. Mapping has one two-second budget captured
at `start`, not renewed on first thread execution. The independent stop handle
revokes synchronously without waiting for that thread, a network round trip, or
a codec. Normal input-agent cleanup still owns release of remotely held keys;
the indicator does not inject input or claim that cancellation is cleanup.

`finish()` is a nonblocking cleanup query. `Some(reason)` proves the UI thread
ended and its owned X resources were released. Retain the owner until then when
cleanup must be proven. Dropping it revokes immediately but cannot interrupt a
hung X server call or prove native cleanup. No asynchronous timeout is presented
as a kill boundary around a blocking foreign call.

## Trust and evidence

The selected desktop user, X server, window manager, compositor and native stack
remain trusted. `_NET_WM_STATE_ABOVE` is a window-manager request, not an enforced
security boundary. A mapped X11 window cannot prove physical/composited visibility.
This is a local revocation surface, NOT a secure approval dialog or a complete
native shell. The application must keep it paired with its original session and
must still supply independently approved observation, capture and control.

The `sharing_indicator_x11` target renders the actual panel under Xvfb and uses
an independent Xlib/XTest peer for pixels, clicks, keys, window hiding, covering,
resizing and destruction. It tests the shared input/observation authority and a
foreign owner with identical IDs, idle expiry, native-open failure and owner drop.
Authority and identity in these tests are explicit fixtures. These tests do not
qualify a desktop window manager, high-DPI display, live tailnet, or human consent.

```sh
FR_NATIVE_INDICATOR_REQUIRED=1 xvfb-run -a \
  -s '-screen 0 1280x1024x24 -noreset -nolisten tcp' \
  cargo test -p fr-native --features linux-session-ui --test sharing_indicator_x11 --locked
```

This advances the sharing-indicator/local-revoke portion of `fr-p1-session-agent-iq3`
and the native shell in `fr-p1-desktop-shell-1-1sq`; neither bead is closed.
