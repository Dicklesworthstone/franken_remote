# Local Stop sharing for hosted desktops

`frd run --observation-indicator /absolute/path/fr-observation-indicator`
requires a live read-only desktop indicator before the host listener starts.
On Linux it covers view-only sharing, including optional host playback audio.
The indicator stays up for the whole enabled host run, including idle listening
between peers; it is not a peer count or evidence that a viewer is connected.

Linux selects this surface explicitly. **macOS always requires it**, for both
view-only and control-capable hosts. The Mac's default image is the matching
`fr-observation-indicator` beside `frd`; the option selects an absolute override.
It does not implement local approval, approve a peer, or change Tailscale
admission. Local approval uses its separate platform-specific consent path.

## macOS hosting

The signed macOS installer builds this helper separately with its minimal
feature selection, then signs and installs it. The helper build does not enable
the input, clipboard, desktop-client or broker/runtime features. A manual build uses:

```sh
cargo build -p fr-native --no-default-features --features macos-observation-indicator --bin fr-observation-indicator --locked
```

Before the host can start listening, the helper must show a **Screen sharing
enabled — click to stop** panel on every original display, up to the protocol's
eight-display bound. This indicates that hosting is enabled, including while
waiting for peers. The native adapter pins the display set and geometry;
hotplug or a geometry change ends this run and requires a new host start.
The control and clipboard panels retain their own separate lifetimes.

Each original Stop control must complete drawing and retain its expected
content view and visible bounds before the panel can establish readiness.
The control is exposed as an accessibility button with a Stop action; that
action only revokes the current lifetime. A late visibility result cannot
overwrite an earlier terminal callback. If a full 32-event turn still leaves
native events queued, the helper refuses readiness rather than postponing a
potential Stop behind the backlog.

AppKit runs only on the child's main thread. A separate command responder and
watchdog check a sleep-inclusive, terminal 250 ms visibility heartbeat. Commands
cannot refresh that evidence. Each readiness reply waits for a completed native
event/visibility turn that began after that request arrived; cached visibility
alone cannot extend the parent's request deadline. A stalled native call,
hidden or lost panel, parent loss or malformed IPC ends the original host.
A local Stop action uses the local-revoke signal; ordinary host cleanup
completes with a successful exit, so launchd's failure-only restart does not
undo that action. The macOS role refuses approval and microphone commands
rather than reusing the observation surface for another capability.

This visible surface complements the independent
[console-session monitor](macos-host.md#console-session-lifecycle). It does not
establish graphical screen-lock detection or grant input authority. AppKit
and window-server visibility checks are not optical proof that a person can
see a panel. Native Mac execution and physical input/visibility qualification
remain separate from source and protocol tests. Tracking: `fr-hjl0`.

## Linux build and run

Build the daemon and the separate native UI image with the repository's pinned
toolchain. The UI reuses the existing `linux-input` native binding feature but
its executable contains no input-executor command path:

```sh
cargo build -p frd --bin frd --locked
cargo build -p fr-native --features linux-input --bin fr-observation-indicator --locked
```

Use the same locally selected display and XAUTHORITY as the capture worker:

```sh
frd run --approval none --software-explicit \
  --display :0 --worker /absolute/path/fr-media-worker \
  --observation-indicator /absolute/path/fr-observation-indicator
```

The usual installed-Tailscale, certificate, media-worker and root/ingress-helper
prerequisites still apply. Add the existing `--audio` and local audio-server
options to include playback audio. This option never enables audio by itself.
`--logind-session` remains the independent lock/logout/suspend boundary.

On Linux, do not combine this slice with `--input-agent`: parsing and the
library entry point refuse that combination. A controller-capable host keeps
its existing mandatory per-lease control indicator. Combining both native
window lifetimes has not been qualified, so it is not silently enabled.

## Lifetime and failure behavior

The Linux child uses the existing X11 Stop sharing surface and its device-attributed
input handling; macOS uses its own AppKit panels. Neither has capture,
clipboard, file-transfer, input-injection or positive-approval protocol.
The parent/child channel is an inherited socketpair, not a listener; every
record has fixed size, direction, sequence and epoch checks. The parent passes
a cleared environment with only the locally selected X11 display and optional
XAUTHORITY on Linux; macOS receives neither variable. The child binds its
lifetime to the parent process.

Native mapping must complete, then a short readiness check must succeed, before
`host_run::run_with_policy` is invoked. The daemon's existing runtime remains on
its original caller thread and stack. A scoped liveness watcher independently
checks the single native-I/O worker while the host runs. Readiness expires at
250 milliseconds from the corresponding request's start, not reply arrival;
partial I/O, scheduling delay and time asleep consume the same budget. Native
event-worker progress is checked separately from its earlier mapped state.

Closing/revoking the UI, a child exit, a malformed reply or expired liveness
requests the ORIGINAL host's cooperative stop. No subsequent sharing session or
replacement UI is started inside that owner. The host drains through its existing
capture/audio/input/firewall cleanup; normal host completion then stops the UI.
The native-I/O worker kills the original child process group before reaping it.
It deliberately does not reap during polling, avoiding process-ID reuse before
signalling. An unfinished or uncertain cleanup retains the bounded worker permit
and prevents replacement work.

An opening failure is a refusal. On macOS, an attributed local Stop during
opening cancels startup successfully without starting the host; launchd cannot
restart it as a failed run. Linux retains its existing refusal for any stopped
UI before first readiness. Diagnostic codes include
`observation_indicator_unavailable`, `observation_indicator_evidence_lost`,
`observation_indicator_protocol`, `observation_indicator_clock_failed`,
`observation_indicator_busy`, and `cleanup_incomplete`. The listening JSON record
includes the additive `observation_indicator` selection field.

## Evidence and limitations

X11 mapping and responsive native code are not optical proof of human visibility.
The selected desktop user, X server and window manager remain trusted; the child
is crash isolation, not a security sandbox against the same user. A headless
Xvfb display is not a physical privacy indicator. macOS retains the same
selected-user and OS trust boundary; there is no Wayland or Windows surface.

Production-code regressions cover fixed framing, correlation, deadline expiry,
late-response non-revival, original stop propagation and cleanup ownership.
Real subprocess fixtures cover terminal signal/EOF ordering and malformed
signals/replies without relabelling a protocol fault as a graceful local Stop.
These fixtures are not native UI or installed-tailnet evidence.

Before an execution-environment outage, the focused supervisor suite passed
18 tests and strict Clippy. The native policy, role and pipe suite passed
17 tests. The published files were recovered from the source and patches after
that outage and independently reviewed; a final full build and formatting pass
on the recovered snapshot remain unverified. Apple SDK compilation and actual
AppKit execution have not been qualified. See the implementation status for
the scope and remaining gates.

Useful focused checks:

```sh
cargo test -p fr-core indicator_process --locked
cargo test -p frd --lib host_indicator --locked
cargo test -p frd --lib observation_indicator --locked
cargo test -p fr-native --features linux-input --lib sharing_indicator::responsiveness --locked
cargo test -p fr-native --features linux-input --bin fr-observation-indicator --locked
cargo test -p fr-native --no-default-features --features macos-observation-indicator --lib macos_observation --locked
cargo test -p fr-native --no-default-features --features macos-observation-indicator --bin fr-observation-indicator --locked
```

The broader remaining work includes physical-input and window-manager behavior,
installed-host qualification, and observation indication on Linux controller-capable
hosts. The macOS CLI source path requires its host-lifetime surface for either role.
