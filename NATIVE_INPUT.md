# Native X11 input boundary

`fr-native` exposes `input::X11Pointer` under the explicit `linux-input` feature.
It calls XTest on a locally selected X11 display and implements the same
`InputSink` consumed by `fr-core::input_submission::InputSession`. It adds no
Rust dependencies, asynchronous runtime, shell command, or public listener.
The narrow FFI uses the system X11 and Linux `libXtst.so.6` ABIs.

The implemented subset is absolute pointer motion and primary, secondary,
middle, back and forward button submission. Keyboard position mapping, keyboard
repeat ownership, committed Unicode, relative motion and scrolling are explicitly
unsupported in this adapter. Their wire/core paths exist but do not manufacture
native support. This is never a fallback after Wayland/portal permission refusal.

## Ownership and failure behavior

The connection is neither Send nor Sync and belongs in the selected user's
interactive input process, not the media worker or broker. Only explicit local
`:display[.screen]` selectors are accepted, never peer-supplied display names.
Geometry preflight happens before the submission owner's final clock check;
changed dimensions refuse coordinate work. Release-only cleanup remains possible
after geometry changes. XTest requests use zero server-side delay and are flushed
before return; success means submitted through the native API, not an observed
application result. `query_pointer` is explicit local instrumentation, not
routine remote telemetry.

Xlib can block or terminate its process on display-server failure. A production
input agent must connect an independent lease/watchdog/revoke path and report
uncertain release if that process fails. This adapter cannot undo already entered
native calls and does not provide a sandbox, secure-desktop access, a lock/session
watcher, or a universal distinction between physical and synthetic held state.
The existing desktop user and X server remain trust boundaries. Deployment must
use protected system/package library paths and a controlled helper environment;
this developer feature is not a signed installation profile.

## Reproduction

Install X11 development libraries, `libxtst6`, Xvfb and the repository-pinned Rust
toolchain. The test launches its own private X servers; it does not touch the
user's current desktop.

```sh
cargo test -p fr-native --features linux-input --test input_x11 --locked
```

The integration tests route actual FRD0 input bytes through the real authority,
replay and final-submission owners into XTest. They query X11 state to check
motion, dragging, old-pointer barriers, duplicate-action suppression, ticket
expiry, release-only cleanup and revoke between position and button submission.
Other cases refuse foreign display selectors, unprepared native requests,
unqualified text, stale viewport generations and off-display coordinates.

These are native API/Xvfb effects with explicitly constructed local test grants,
not Tailscale admission, physical-device input latency, real-compositor permission
qualification or a complete remote-desktop application. Dynamic display resize,
button remapping, physical/synthetic collisions and process-death release behavior
remain qualification work. Run the full native workspace lane separately:

```sh
./scripts/verify.sh fast
./scripts/verify.sh docs
```
