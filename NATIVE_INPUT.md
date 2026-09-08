# Native X11 input boundary

`fr-native` exposes `input::X11Pointer` under the explicit `linux-input` feature.
It calls XTest on a locally selected X11 display and implements the same
`InputSink` consumed by `fr-core::input_submission::InputSession`. It adds no
Rust dependencies, asynchronous runtime, shell command, or public listener.
The narrow FFI uses the system X11 and Linux `libXtst.so.6` ABIs; physical-key
metadata is accessed by the small XKB ABI helper built against system headers.
**Asupersync QUIC remains the primary transport.** Nothing here changes that
selection or replaces the upstream QUIC work.

The implemented subset is absolute pointer motion, mapped primary/secondary/
middle/back/forward buttons, physical keyboard press/release and client-owned
repeat. Physical keys are resolved through XKB key names rather than character
layout guesses. See [XKB_INPUT.md](XKB_INPUT.md) for the keyboard preparation,
repeat and cleanup contract. Committed Unicode, relative motion and scrolling
remain explicitly unsupported by this native adapter; their wire/core paths do
not manufacture native support. There is no clipboard-paste fallback and no
fallback after Wayland/portal permission refusal.

## Ownership and failure behavior

The connection is neither Send nor Sync and belongs in the selected user's
interactive input process, not the media worker or broker. Only explicit local
`:display[.screen]` selectors are accepted, never peer-supplied display names.
Geometry, XKB and button-map preflight happen before the submission owner's
final clock check. Changed dimensions refuse new coordinate work. Release-only
cleanup remains possible after geometry changes. XTest requests use zero
server-side delay and are flushed before return; success means native API
submission, not an observed application result. `query_pointer` is explicit
local instrumentation, not routine remote telemetry.

The adapter records each native keycode/button code before a possible press.
Release uses that same physical code, not a new lookup after remapping. Button
mapping is read and inverted per press, so a logical primary click does not
become secondary on a swapped mapping. Missing/disabled/ambiguous mappings
refuse. Pre-existing presses of core buttons and supported keys are refused
rather than claimed as this owner's held input. Extended back/forward buttons
have no held-state bit in core XQueryPointer, so equivalent pre-existing-state
attribution is not claimed for them.

Normal native-owner Drop attempts release/restoration of both keys and drags,
including the path where an enclosing owner forgot an explicit cleanup call.
For acknowledged handoff, first retire input authority, then require both the
core cleanup result to have zero remaining held items and
`X11Pointer::cleanup_native()` to return true. Uncertain native cleanup remains
tracked for explicit local retry. `cleanup_keyboard()` addresses only the
keyboard subset and is not a complete handoff check. Cleanup never presses,
moves or resurrects a remote action. A failed XTest request reports unknown
effect instead of discarding a possible press from the cleanup ledger.

Xlib can block or terminate its process on display-server failure. A production
input agent must connect an independent lease/watchdog/revoke path and report
uncertain release if that process fails. This adapter cannot undo already entered
native calls and does not provide a sandbox, secure-desktop access, a lock/session
watcher, or a universal distinction between physical and synthetic held state.
Per-key repeat settings are temporarily X-server-global; abrupt process death
can leave restoration uncertain. The existing desktop user and X server remain
trust boundaries. Deployment must use protected system/package library paths
and a controlled helper environment; this developer feature is not a signed
installation profile.

## Reproduction and evidence

Install X11 development libraries, `libxtst6`, Xvfb and the repository-pinned Rust
toolchain. Tests launch private X servers; they do not touch the user's desktop.
The `linux-input` feature alone does not build or link FFmpeg.

```sh
cargo test -p fr-native --features linux-input --test input_x11 --locked
cargo test -p fr-native --features linux-input --test keyboard_x11 --locked -- --test-threads=1
```

All **13 native integration tests passed locally**: four existing pointer tests
unchanged and nine keyboard/ownership tests. Each passes actual FRD0 bytes through
real authority/replay/final-submission logic into XTest, then queries X11 or uses
an independent Xlib client to observe events/state. Covered behavior includes
keys, repeat, dragging, old-pointer barriers, duplicate suppression, native
preparation expiry, partial repeat effects, revoke during preparation, keymap
changes, all five remapped buttons, pre-existing local input, and Drop cleanup.
Native library Cargo Clippy and pinned Clippy checks of both native test binaries
passed without weakening warnings or assertions.

The same core/wire/media sources passed **162 Cargo tests/doctests**, including
five new native-preparation/compound-repeat fault regressions. These local native
checks linked the actual Cargo-built first-party libraries with the pinned
compiler; they are not a claim that the complete runtime/native workspace was
rebuilt locally. Full workspace/CI results must stay pinned to their source run.

These are native API/Xvfb effects with explicit local test grants, not Tailscale
admission, physical-device input latency, real-compositor permission qualification,
or a complete remote-desktop application. Dynamic display resize, simultaneous
physical/synthetic collisions and abrupt process-death recovery still need live
qualification. Run the full native workspace lane separately:

```sh
./scripts/verify.sh fast
./scripts/verify.sh docs
```
