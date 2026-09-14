# Native X11 input boundary

`fr-native` exposes `input::X11Pointer` under the explicit `linux-input` feature.
It calls XTest on a locally selected X11 display and implements the same
`InputSink` consumed by `fr-core::input_submission::InputSession`. It adds no
Rust dependencies, asynchronous runtime, shell command, or public listener.
The narrow FFI uses the system X11 and Linux `libXtst.so.6` ABIs; physical-key
metadata is accessed by the small XKB ABI helper built against system headers.
**Asupersync QUIC remains the primary transport.** Nothing here changes that
selection or replaces the upstream QUIC work.

The implemented subset is absolute pointer motion, bounded relative motion on
a qualified single-X-screen connection, mapped primary/secondary/middle/back/
forward buttons, physical keyboard press/release and client-owned repeat.
Physical keys are resolved through XKB key names rather than character layout
guesses. See [XKB_INPUT.md](XKB_INPUT.md) for the keyboard preparation,
repeat and cleanup contract. Committed Unicode and scrolling remain explicitly
unsupported by this native adapter; their wire/core paths do not manufacture
native support. There is no clipboard-paste fallback and no
fallback after Wayland/portal permission refusal.

## Relative motion

A successful XTest version query enables `Capability::Relative` only for version
2.1 or later within major version 2 and exactly one X screen in the connection
setup. The native relative entry has no screen selector and addresses the
current pointer root. Multi-X-screen connections therefore retain their absolute
input support but refuse relative operations, rather than borrowing an unrelated
root. This restriction is about X screens, not the number of RandR monitors.

The existing `RelativeCheckpoint` record stays reliable and ordered. The core
owner converts cumulative i64 positions to checked deltas in the current mode
epoch; the adapter additionally checks XTest's signed-16-bit wire range before
passing C integers to the library. There is no truncation, delta splitting,
synthetic acceleration, delayed replay or conversion to an absolute warp. A
zero checkpoint delta needs no OS call. Replays return the retained result and
cannot move the pointer twice; ticket rollover does not reset the cumulative
position. An input-mode change still requires a fresh ticket.

Preflight checks current geometry and rejects a known off-display destination
instead of deliberately asking XTest to clamp it. The actual command remains
relative to the pointer at submission, so intervening local movement is not
overwritten by a stale absolute target. The existing final authority check runs
after this preflight and foreign-cache lock acquisition. Expiry or local revoke
at that boundary cancels preparation before any relative OS submission. One
accepted nonzero delta is one native request with zero server-side delay.

Local movement, pointer grabs, confinement and display reconfiguration can still
race a submitted X request. This is neither raw-device relative input nor an
atomic physical-position guarantee. Client GUI mode switching/pointer locking,
Wayland relative input and multi-X-screen relative routing are not implemented
by this adapter change. The normal high-level viewer still needs its own
relative-mode integration; exposing a native capability does not supply it.

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

The original keyboard-boundary validation recorded **13 native integration
tests passed locally**: four existing pointer tests unchanged and nine
keyboard/ownership tests. Each passes actual FRD0 bytes through
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

### Relative-input validation

The relative-input increment adds seven native integration cases and one local
capability-rule test. The scoped native run executes **23 tests, zero failures
and zero ignored tests**: two library tests, eleven pointer/input cases, nine
keyboard cases and the existing concurrent-extension-cache test. It passes with
one, four and eight test threads. Strict Clippy and formatting pass without
relaxing warnings or expiry/replay assertions.

The new cases exercise actual FRD0 encode/decode, production cumulative input
and final-submission ownership, real XTest requests and independent X11 pointer
queries. They cover reverse/zero/repeated checkpoints, ticket rollover, fresh
mode tickets, stale view/mode identities, reliable sequence gaps, native integer
bounds, known off-display destinations, expiry and revoke after preparation,
local pointer movement between preparation and submission, canceled preparation
and explicit multi-X-screen refusal. Grants, view readiness and host time in
these tests remain labeled fixtures; the X server and input effects are real.

This local run uses the repository-pinned nightly-2026-08-31 compiler and an
external scoped Cargo manifest pointing at the unchanged native build script,
updated native sources/tests and hash-matched core/wire sources from `13a40a1`.
It rebuilds those first-party libraries and the XKB helper, not the full
Asupersync/media workspace. No full-workspace CI, physical display/input-latency,
GUI-relative-mode or Tailscale admission result is claimed for this increment.
The normal repository commands for the same test targets are:

```sh
cargo test -p fr-native --features linux-input --lib --test input_x11 --test keyboard_x11 --test input_concurrency --locked -- --test-threads=4
cargo clippy -p fr-native --features linux-input --lib --test input_x11 --test keyboard_x11 --test input_concurrency --locked -- -D warnings
```

This advances `fr-p1-input-pipeline-ay1` without closing its broader acceptance
criteria. No dependency, codec, runtime, wire format or release pin is changed.
