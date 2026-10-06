# Optional on-desktop Stop sharing for observation-only hosts

`frd run --observation-indicator /absolute/path/fr-observation-indicator`
requires a live read-only desktop indicator before the host listener starts.
It covers view-only sharing, including optional host playback audio. The
indicator stays up for the whole enabled host run, including idle listening
between peers; it is not a peer count or evidence that a viewer is connected.

This is an explicit opt-in. Without it, the original host-run behavior is
unchanged. It does not implement local approval, approve a peer, or change
Tailscale admission. `--approval local` still has its existing refusal.

## Build and run

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

Do not combine this slice with `--input-agent`: parsing and the library entry
point refuse that combination. A controller-capable host keeps its existing
mandatory per-lease control indicator. Combining both native window lifetimes
has not been qualified, so it is not silently enabled.

## Lifetime and failure behavior

The child uses the existing X11 Stop sharing surface and its device-attributed
input handling. It has no capture, clipboard, file-transfer, input-injection or
positive-approval protocol. The parent/child channel is an inherited socketpair,
not a listener; every record has fixed size, direction, sequence and epoch checks.
The parent passes a cleared environment with only the locally selected display
and optional XAUTHORITY, and the child binds its lifetime to the parent process.

Native mapping must complete, then a short readiness check must succeed, before
`host_run::run_with_policy` is invoked. The daemon's existing runtime remains on
its original caller thread and stack. A scoped liveness watcher independently
checks the single native-I/O worker while the host runs. Readiness expires at
250 milliseconds from the corresponding request's start, not reply arrival;
partial I/O and scheduling delay consume the same budget. Native event-worker
progress is also checked separately from its earlier mapped state.

Closing/revoking the UI, a child exit, a malformed reply or expired liveness
requests the ORIGINAL host's cooperative stop. No subsequent sharing session or
replacement UI is automatically started. The host drains through its existing
capture/audio/input/firewall cleanup; normal host completion then stops the UI.
The native-I/O worker kills the original child process group before reaping it.
It deliberately does not reap during polling, avoiding process-ID reuse before
signalling. An unfinished or uncertain cleanup retains the bounded worker permit
and prevents replacement work.

An opening failure is a refusal, even if the child reports local revocation
while it opens. It cannot be mistaken for successful host startup. Diagnostic
codes include `observation_indicator_unavailable`,
`observation_indicator_evidence_lost`, `observation_indicator_protocol`,
`observation_indicator_busy`, and `cleanup_incomplete`. The listening JSON record
includes the additive `observation_indicator` selection field.

## Evidence and limitations

X11 mapping and responsive native code are not optical proof of human visibility.
The selected desktop user, X server and window manager remain trusted; the child
is crash isolation, not a security sandbox against the same user. A headless
Xvfb display is not a physical privacy indicator. No Wayland/macOS/Windows UI is
added here.

Production-code regressions cover fixed framing, correlation, deadline expiry,
late-response non-revival, original stop propagation and cleanup ownership. A
protocol-fixture subprocess exercises startup refusal, fragmentation, wrong
epoch/sequence, EOF, initial and post-start stalls, local revocation, and complete
host teardown. These fixtures are not native UI or installed-tailnet evidence.

Useful focused checks:

```sh
cargo test -p fr-core indicator_process --locked
cargo test -p frd --lib host_indicator --locked
cargo test -p frd --lib observation_indicator --locked
cargo test -p fr-native --features linux-input --lib sharing_indicator::responsiveness --locked
cargo test -p fr-native --features linux-input --bin fr-observation-indicator --locked
```

The remaining broader observation-indicator bead stays open for native child,
physical-input/window-manager, installed-host/namespace qualification and eventual
coverage of observer sessions on controller-capable hosts.
