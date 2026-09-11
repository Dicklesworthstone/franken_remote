# Reconciled screen discovery and selected-monitor capture

The existing full-X11-screen capture path and the RandR monitor path share one
private worker protocol, initialization dispatcher, process supervisor and codec
pipeline. Neither implementation replaces the other. Asupersync remains the only
runtime and primary QUIC transport. There is no additional video codec, third-party
Rust dependency, listener or identity mechanism.

## Two explicit capture profiles, one worker lifecycle

`Worker::discover_capture` and `CaptureDiscovery` retain their existing APIs and
private records. They enumerate full X11 screens and configure the exact retained
screen index/root/dimensions on the same child and X connection. Their original
record numbers and golden encodings are unchanged.

`MonitorDiscovery` is the additional owner for opt-in `linux-displays` builds. It
enumerates active RandR 1.5 monitors within the explicitly local X11 screen, then
configures only the selected monitor rectangle. Its records are nested under the
canonical `fr_media::worker::capture::monitors` module. It uses `Worker::spawn`,
`exchange`, request sequencing, cancellation, abort and reaping instead of another
process-launch or supervision implementation.

The native child handles both profiles in one startup dispatcher. Configuration
for one profile is invalid during discovery of the other. A build without
`linux-displays` refuses monitor discovery as unsupported; it never silently
captures the root framebuffer as a substitute. Existing direct Configure and
ConfigureDecoder operations are preserved.

## Selected output reaches the actual pixels

```text
approved observation -> supervised MonitorDiscovery -> native RandR inventory
  -> bounded private catalog -> parent-owned disclosure aliases
  -> existing DisplayCatalog / explicit SelectDisplay exchange over QUIC
  -> original selection + authority + connection checks
  -> configure that same child and original X connection
  -> capture only the selected rectangle -> HEVC -> negotiated media channels
  -> native decoding/presentation -> pixel readback and dependent-frame repair
```

`DiscoveredSource` joins private discovery to the existing approved observation.
Its configure operation checks the original selection synchronously and returns
a future that borrows neither QUIC nor the connection driver. The enclosing
application must keep servicing the canonical session during native operations.
Discovery and Xlib/codec calls run in the supervised child, not under an authority
lock. A child process provides crash/hang containment, not a security-sandbox claim.

A catalog contains at most eight monitors and eight output identifiers per monitor.
Invalid or excess metadata refuses rather than silently truncating the result.
Native atoms, connector names, paths and output identifiers are not disclosed as
network aliases. A non-wrapping process-local namespace prevents replacement
inventories from reusing an old choice, even with identical native metadata.
Those aliases are not bearer credentials; the existing complete session binding
and actual shared observation-authority object remain mandatory.

`fr_x11_capture_rectangle` validates root bounds and reads only the selected
rectangle. Other monitors are not captured and relabelled as the selected output.
The explicit initial profile requires true-color 24-bit X11 and even dimensions
of at least 16 pixels. Logical coordinates are X root-window pixel units, scale
1:1, already in root orientation: not toolkit DPI or physical-panel measurements.
Host-local Launch selects the X server/screen; no peer string becomes a selector,
command, executable or library path. `libXrandr.so.2` is a native runtime dependency
only for the opt-in monitor feature.

## Topology and output publication

Both capture profiles revalidate topology after encoding, immediately before
releasing the encoded access unit. CaptureSurface::Root uses the existing sticky
X11 geometry/event check; CaptureSurface::Selected validates its retained RandR
inventory. An image submitted before a topology change cannot emerge afterwards
as output for the old selection. Checks also surround readback and precede
unchanged-source receipts. A failed check ends the capture lifetime; restoring
identical dimensions does not revive it.

The monitor inventory retains RandR and root ConfigureNotify events, detecting
remove/recreate changes even when final IDs and dimensions are identical. The
initial monitor profile retires its inventory on any observed topology change,
including one to an unselected monitor. A new checked selection/session is needed;
seamless hotplug and in-place monitor switching are not implemented. Event handling
is bounded to 128 queued events, with excess refused.

Idle check_display/check_selected_display operations perform topology validation
without capturing another image or encoding dummy video. They do not advance pixel
freshness, source-observation age, input tickets or presentation readiness. Actual
unchanged-source reports still require a fresh selected-rectangle readback and
exact comparison with a successfully encoded baseline.

Selected capture/configuration retains actual authority identity, connection and
selected geometry, not just equal numeric IDs. Failed or abandoned selected work
revokes that original observation lifetime. A foreign authority cannot invalidate
another session. The independent input owner remains responsible for its final
OS-submission checks and release-only keyboard/button cleanup.

## Stable private-worker encodings

The existing worker header, epoch and sequences are unchanged. Records are
profile-specific, with one global Kind namespace and explicit body validation.

| Profile | Request / reply | Kind values | Body |
|---|---|---|---|
| Existing full screen | DiscoverCapture / CaptureScreens | 9 / 267 | Empty request; bounded screen catalog |
| Existing full screen | ConfigureCapture / CaptureReady | 10 / 268 | Exact 48-byte configuration echo |
| Selected monitor | DiscoverMonitors / CaptureMonitors | 11 / 269 | Empty request; catalog at most 465 bytes |
| Selected monitor | ConfigureMonitor / MonitorReady | 12 / 270 | Exact 52-byte configuration echo |
| Selected monitor | CheckMonitor / MonitorValid | 13 / 271 | Empty |

The screen configuration retains its original 28-byte codec configuration plus
20-byte screen identity. The monitor configuration contains that codec setup plus
catalog revision u64 and alias u128. Monitor catalogs are bounded to 465 bytes.
Cross-profile kinds and configurations refuse; record lengths, geometry and
limits are checked before native configuration. These are private IPC additions,
not a competing public desktop protocol.

## Reconciliation evidence

The baseline is main at `2a161ba51efcbd65830974db30ae15bdb664e661`, preserving the
published full-screen worker, controlled-host, broker, display-selection and
viewer-prerequisite changes; unrelated staged workflows remain unchanged.
Twenty-two code/configuration/test files comprise
the reconciliation; Cargo.lock and the pinned compiler/runtime versions are unchanged.

The final local selections pass 471 distinct tests with zero failures or ignored
cases: 372 core/wire/media/client Cargo tests, 39 daemon unit tests, 13 daemon
supervision/selection tests, 12 native inventory tests, 14 native worker tests,
20 native decoder-startup tests and one feature-disabled monitor-refusal test.
The feature-disabled run also reruns the 12 existing worker tests. These repeated
cases are not counted twice. Twenty-six cases are additional to the baseline.

The three native suites also pass with one, four and eight threads. A separately
built first increment (before the higher-level selected-capture join) passes its
12 inventory, 14 worker and 14 unchanged decoder-startup tests independently.
Source/test strict Clippy, workspace formatting and documentation checks pass.
Local runtime-bound tests freshly rebuild first-party sources against matching
retained Asupersync libraries; they are not cold full-dependency Cargo builds.

A fresh negative-control build removes only the final post-encode topology check.
Both unchanged after-encode regressions then fail because stale pictures are
released, for root capture as well as monitor capture. The production build passes
both. The negative-control source and failures are retained separately and are
not production changes or passing-test evidence.

Tests use actual Xvfb/RandR, supervised process IPC, software HEVC, localhost
Asupersync QUIC/UDP/TLS and decoded/presented pixel readback. Different pixels on
two logical monitors prove selected-rectangle isolation, and excluded-monitor
changes do not generate unnecessary encoded pictures. Native startup and complete
final-picture repair use discovered and network-selected monitor metadata.
Identity/admission, initial approval and session-control setup remain fixtures.
These results do not qualify physical monitors, GPU execution, WAN conditions or
an installable remote-desktop application. UBS and Beads tooling were unavailable;
no phase or issue was marked completed.

## Publication and clean CI

The capture/profile reconciliation is published in `f19874b`, followed by the
selected-source/native-media integration in `d6e0746`, directly on main without
history rewrites. The exact 22 source objects passed clean pinned-workspace
formatting, compilation, strict Clippy, tests, example tests and documentation
checks in [run 34553382976](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34553382976)
against `be747181`. The focused native suites and feature-disabled profile also
passed before immutable source objects were exported and published.

The verification workflow now checks committed source read-only. It does not
replay patches, export objects, hold a write token or mutate branches. A subsequent
combined checkout's verification status remains separate from that successful
exact-source candidate. Unrelated staged workflows were not changed or applied.

## Reproduction

Use the repository-pinned compiler, FFmpeg/X11 development SDKs, Xvfb, OpenSSL,
and system libXrandr.so.2 for the monitor feature.

```sh
cargo test -p fr-media --test capture_discovery --test worker_displays --locked
cargo test -p fr-native --all-features --test display_inventory --test worker_process --test decoder_startup --locked
cargo test -p fr-native --features linux-media --test worker_process --locked
cargo test -p frd --lib --locked
cargo test -p frd --test worker_supervision --test display_selection --locked
./scripts/verify.sh fast
./scripts/verify.sh docs
```

Related: [DISPLAY_SELECTION.md](DISPLAY_SELECTION.md),
[NATIVE_MEDIA_ATTACHMENT.md](NATIVE_MEDIA_ATTACHMENT.md),
[DECODER_STARTUP.md](DECODER_STARTUP.md), [SESSION_DRIVERS.md](SESSION_DRIVERS.md).
