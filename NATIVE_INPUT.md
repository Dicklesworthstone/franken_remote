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
forward buttons, bounded whole-line wheel scrolling, physical keyboard
press/release and client-owned repeat.
Physical keys are resolved through XKB key names rather than character layout
guesses. See [XKB_INPUT.md](XKB_INPUT.md) for the keyboard preparation,
repeat and cleanup contract. [NATIVE_SCROLL.md](NATIVE_SCROLL.md) specifies the
whole-line scroll bound and separately authorized wheel transitions. Committed
Unicode, fractional-line and pixel scrolling remain unsupported by this native
adapter; their wire/core paths do not manufacture native support. There is no
clipboard-paste fallback and no
fallback after Wayland/portal permission refusal.

## Selected monitor control

The `linux-input-monitors` feature admits a whole RandR 1.5 monitor without
enabling FFmpeg, capture or DAMAGE. A combined `linux-displays,linux-input`
build provides the same input path. Plain `linux-input` keeps its existing
whole-root profile and does not acquire a RandR runtime dependency.

In a monitor-enabled build, `X11Pointer::open_exact` preserves the granted
monitor's origin and dimensions. It opens the canonical bounded `X11Inventory`
and requires exactly one current monitor with those bounds, including when the
selected monitor covers the entire X root.
Arbitrary crops, a rectangle spanning neighboring monitors, and ambiguous
equal-bounds monitors refuse. There is no fallback to root-sized authority.
The explicit `X11Pointer::open` root/probe API and plain `linux-input` build
retain their original whole-root profile. A monitor-enabled exact bind never
takes that path after a missing or ambiguous monitor match.

Matching rectangles are not sufficient to prove that the input process opened
the monitor previously selected for capture. The original capture inventory
must revalidate **after the input executor reports Ready and before the host
publishes its grant**. The controlled source performs that check on its original
worker and retained selection. The input process then keeps its own inventory
alive throughout the grant. Their overlapping lifetimes prevent an equal-size
remove/re-add between discovery and native startup from becoming a new target
under the old selection.

Absolute input already contains X root-window pixel coordinates. The adapter
does not subtract or add the selected origin a second time. Pointer destinations
must lie inside the granted rectangle, with exclusive right and bottom edges.
Button and wheel presses also check the actual current pointer against that
rectangle during preflight; intervening local movement to a neighboring monitor
does not authorize a new press there. Existing wire coordinate/barrier rules and
the final host ticket/authority checks remain in effect for each native operation.

Before positive key, pointer, button and wheel work on an exact monitor target,
preflight checks root geometry and the retained inventory. Any observed topology change is terminal,
including a neighbor changing or a monitor being removed and recreated with
the same native identity and bounds. Restoring the old layout does not revive
the owner. Release-only cleanup remains available after that fence, including
when the local pointer has moved outside the selected rectangle.

The native owner refreshes a prepared operation again at submission: an earlier
`Prepared` reply does not preserve topology, pointer position or a button/key
mapping across an IPC delay. It releases the XTest cache guard before those
queries while retaining consent exclusion, then reacquires the guard before the
final authority predicate. The canonical `InputSession` supplies its fresh
authority/clock check through `submit_authorized`; the separate executor supplies
its fence, indicator and monotonic deadline check through `submit_checked`.
Its emergency release snapshot includes any physical codes refreshed by this
preparation. A refusal consumes the preparation and makes no native input call.

The shared owner samples its host clock again after policy authorization has
released every authority lock. This accounts for time spent waiting for policy
access, even when no policy writer advances its clock. That last sample must
precede the original exclusive submission deadline; neither lock contention nor
revalidation renews it. Clock regression and a concurrent revoke refuse the
action before speculative held-state tracking. Previously submitted operations
keep their partial receipt and release-only cleanup; the action is not replayed.

The blocking native checks finish before that final authorization/clock sample.
No server grab or blocking preflight follows it. As elsewhere in this adapter,
local pointer movement and X11
reconfiguration can race an already submitted request; this is not atomic
physical-position enforcement. Keyboard injection retains the X server's current
focus semantics. Monitor selection is a viewing/coordinate binding, not an
application isolation boundary. The selected desktop user and X server remain
trusted. Existing root-size, native-monitor format and capability bounds apply;
seamless hotplug and in-place target switching remain unavailable.

### Input deadlines across system suspend

The separate input executor checks positive submissions against a clock that
includes system suspend: `CLOCK_BOOTTIME` on Linux and `CLOCK_MONOTONIC` on
macOS. The broker retains one conservative correlation between that clock and
its runtime clock before the original executor factory can wait for its native
thread or launch a child. Each translated
deadline is capped by both that original correlation and a fresh sample. A
runtime clock that pauses during sleep therefore cannot give an old action a
new lifetime when the broker translates it after wake. A command already in the
private IPC queue also expires while the machine sleeps.

The final check still follows native event preparation and requires a clock
reading strictly before the original exclusive deadline. Missing or regressed
clock evidence, arithmetic overflow, expired work and mismatched private IPC
versions refuse submission. The private input protocol is version 2; a broker
and executor from different protocol generations cannot interpret one another's
deadline origins. Release-only cleanup remains available after expiry and
does not replay a press or other positive action.

The deterministic clock tests model suspend both before deadline translation
and after a command has been prepared. A real kernel-clock check verifies the
selected Linux clock domain. Those checks do not establish physical
suspend/resume behavior, graphical-session lock handling, or native XTest and
Quartz effects; those require their existing platform qualification lanes.

The native regression `input_monitors_x11` uses real RandR monitors and XTest on
private Xvfb servers, with an independent Xlib observer. It exercises nonzero
horizontal and vertical origins, actual button/wheel event coordinates, crop and
ambiguity refusal (including root-sized monitors), neighboring pixels, local pointer escape, sticky topology
fences, release-only cleanup, and the original inventory's post-startup check.
It also separates Prepare from Submit, changes topology or moves the pointer in
between, and verifies refusal both directly and over the actual input-agent's
private IPC. The child additionally refuses an expired prepared request without
replaying it. Authority and clock values in the core-path tests are explicit
local fixtures, not live tailnet or physical-display qualification.

```sh
cargo build -p fr-native --lib --no-default-features --features linux-input-monitors --locked
cargo test -p fr-native --no-default-features --features linux-input-monitors --test input_monitors_x11 --locked -- --test-threads=1
```

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

Normal native-owner Drop attempts release/restoration of keys, drags and any
possibly held wheel transition,
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
