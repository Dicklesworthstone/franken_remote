# Local desktop approval

`frd run --approval local` asks at the shared desktop before exposing it: an
X11 consent surface on Linux, or a dedicated AppKit window on macOS. It
distinguishes viewing from a session requesting control. The result applies
only to the original pending connection; it is not approval of future peers,
another share, or a reusable input lease.

## Linux: build and select the local desktop

Build the daemon, capture worker, session monitor and consent UI with the
repository-pinned toolchain and the existing native SDK prerequisites:

```sh
cargo build -p frd --bin frd --locked
cargo build -p fr-native --features linux-displays,linux-input,linux-session-monitor --bins --locked
```

The CLI loads `fr-observation-indicator` from the SAME directory as the running
`frd` executable. This is the packaged native session-UI image in its separate
one-use consent role. It is not a peer-selected command and opens no listener.
Install the trusted matching images together; the library embedding API accepts
an explicit local `host_run::approval::Configuration` instead.

Run as the selected desktop's user, with the usual ingress helper/root setup and
installed Tailscale prerequisites. Select the actual logind session matching the
display and user; no session is guessed from a remote request:

```sh
frd run --approval local --encoder software \
  --display :0 --logind-session c2 \
  --worker /absolute/path/fr-media-worker \
  --session-monitor /absolute/path/fr-session-monitor
```

Use the actual local session ID, not the example `c2`. The monitor must produce
fresh evidence for this exact display/user before the host listens. Lock, logout,
switch-away, suspend and lost evidence end the original share and host run. No
new session automatically inherits consent.

For control, add the existing `--input-agent /absolute/path/fr-input-agent`.
The prompt covers control intent for this session. Actual input still requires
its separately checked target, capabilities, fresh usable view, exclusive seat
and native readiness. The per-lease control indicator remains mandatory. Audio,
clipboard, file reception, encoder, frame rate and bitrate remain independent
local settings and negotiated capabilities; selecting local approval enables
none of them.

Headless mode, missing logind selection, a missing packaged UI image and the
separate `--observation-indicator` wrapper refuse this consent profile with
`local_approval_unavailable`. There is no stdin/terminal fallback or automatic
switch to unattended sharing. The long-lived observation-indicator window and
the one-use consent window are not combined by this slice.

## Linux: install the monitored host as a user service

The installer accepts the same explicit desktop selection for a systemd user
service. For example, preview the service for your actual selected session:

```sh
frd install --user --dry-run --approval local --sharing own-user -- \
  --encoder software --display :0 --logind-session c2 \
  --worker /absolute/path/fr-media-worker \
  --session-monitor /absolute/path/fr-session-monitor
```

Remove `--dry-run` to write the service definition, then follow the reported
service-manager and ingress-helper steps. Installation does not start sharing
or approve a connection. Add `--input-agent /absolute/path/fr-input-agent` after
`--` if this host should also offer control; local approval alone enables no
input, audio, clipboard or file capability.

The unit starts with and belongs to `graphical-session.target`. Supply that
desktop's X11 environment to the user service and install the matching
`fr-observation-indicator` beside `frd`. Every start still verifies the selected
logind session and the packaged consent UI before listening. The session ID is
an explicit selection, not a reusable login identity: if a later login changes
it, update the service selection. No new desktop inherits an earlier approval.

A missing `--logind-session`, system service, headless profile or separate
`--observation-indicator` wrapper refuses explicit local-approval installation.
Omitting `--approval` continues to inherit saved policy at host startup; the
installer neither rewrites that policy nor changes it to unattended sharing.
These installer checks verify the configuration, not device consent or a live
installed desktop.

## macOS: approve in the original console session

Build `frd` with `macos-host`, and install the matching signed worker, input
agent, session monitor and UI images as described in [macOS hosting](macos-host.md).
The existing `macos-observation-indicator` helper feature includes both the
long-lived Stop Sharing role and this separate, one-use approval role. For
example, on the logged-in Mac:

```sh
frd run --approval local --sharing own-user --encoder videotoolbox \
  --worker /absolute/path/fr-media-worker \
  --session-monitor /absolute/path/fr-session-monitor
```

Add `--input-agent /absolute/path/fr-input-agent` to offer control. The UI
defaults to `fr-observation-indicator` beside `frd`; an explicit
`--observation-indicator /absolute/path/fr-observation-indicator` selects the
trusted matching image for both roles. They run in separate original children.
The mandatory menu-bar Stop Sharing item remains alive throughout hosting,
including while approval is pending. Its stop or lost evidence still ends the
host. Selecting local approval does not bypass the console-session monitor,
Screen Recording or Accessibility permission, or the control/clipboard indicators.

The prompt requires a completed draw, the original visible focused window, and
a fresh matching pointer-down/up gesture inside **Allow**. Activation clicks,
keyboard shortcuts, accessibility press actions, stale events, process-attributed
events, private event-state sources and FrankenRemote-tagged remote input cannot
produce a positive answer. **Deny**, Escape, closing, lost focus, session
deactivation, sleep, an event backlog or stalled native progress refuse the
original prompt. Source metadata is a conservative filter, not a proof that an
event came from physical hardware: Core Graphics event-state sources can be
constructed. The selected local user and OS remain trusted.

To preview a per-user LaunchAgent with local approval:

```sh
frd install --user --dry-run --approval local --sharing own-user -- \
  --encoder videotoolbox --worker /absolute/path/fr-media-worker \
  --session-monitor /absolute/path/fr-session-monitor
```

Remove `--dry-run` to write the plist and follow its reported launchctl steps.
Installation grants no consent and starts no sharing. A LaunchDaemon, X11
display/headless options and Linux logind selection remain unavailable. The
installer script's existing `--approval none --sharing own-user` default is
unchanged; the explicit command above selects local approval. This prompt does
not establish graphical screen-lock detection.

The Mac prompt identifies the authenticated Tailscale node by its complete
stable ID, with the installed daemon's device alias when available. The alias
is a display label; the node ID distinguishes devices with the same name.
The prompt also shows the original requested role and the negotiated playback
audio, clipboard, file-send, file-receive and microphone capabilities. Playback
audio explicitly includes other applications and displays when selected. The
operator must enable it with `--audio`, and the viewer must negotiate it;
that choice never enables a microphone.

These details are an immutable snapshot of the original authenticated admission
and negotiation. A bounded private record binds them to the exact prompt's
opening kind, sequence and epoch. Missing, malformed, mismatched or unsafe text
refuses before a window opens. The helper copies the complete disclosure and
checks that it fits; it cannot approve a clipped or partly off-screen prompt.
The local request reference is a display-only ordinal, not a session nonce,
attachment token or new approval capability.

The initial display catalog and monitor selection still occur after approval.
The prompt states that ordering and identifies the host login desktop as its
scope; it does not promise approval tied to a preselected monitor. Control can
affect the wider login desktop. Binding consent to a specific preselected
monitor remains separate work.

## Original lifetimes, not reusable approvals

The host notifies a capacity-one inbox with the ORIGINAL `Approval` capability,
then continues its existing admission and local-session servicing. Only the
original host's local-priority turn may consume a native decision. Stop, current
selected native-session evidence and policy revision are checked before that
turn; host startup
independently rechecks current tailnet admission and transport before granting
observation. Capture setup is invoked only after the approved session completes.

On Linux the native child reuses the existing XI2 source-device attribution and
whole-display input exclusion. XTest, forged core events and mapping alone cannot
supply a positive answer. A completed draw must precede an Allow gesture. Denial
or hiding in the same bounded event turn takes precedence. The child destroys
its prompt before returning a positive decision. The parent then reaps that
exact child and joins its worker before consuming the original capability.

The Mac's main-thread AppKit surface has a separate IPC/watchdog thread. Each
readiness reply requires a native turn that began after that request; a command
cannot refresh cached visibility. Its 250 ms evidence budget uses Darwin's
sleep-inclusive clock. A tentative Allow expires if native destruction stalls,
the original prompt is cancelled, or that original deadline passes. The window
must be destroyed before a positive reply, and the common parent then requires
the exact child reap before consuming the original `Approval`.

One original 30-second host startup budget includes negotiation and consent.
The native prompt has an additional 30-second upper bound, but cannot extend the
original host deadline. The native client's default total bootstrap budget is
30 seconds as well; it includes approval, display choice and first decode.
Explicit shorter client policies remain shorter. Per-stage native deadlines
remain two seconds and active observation/input leases, tickets, transport
record lifetimes and the control-grant exchange are unchanged. Neither a byte,
UI update, notification nor a late positive result refreshes these deadlines.

A delivered but unconsumed decision still occupies the one pending slot. A
cancelled or expired original cannot be replaced by a new owner with equal
numeric identifiers. Every exit fences the prompt before cleanup waits, and a
pending or uncertain native reap retains its global worker capacity instead of
starting another worker. Failed cleanup is reported as `cleanup_incomplete`.

On Linux, saved policy still controls each new share epoch. When the CLI has the matching
logind selection and packaged UI available at startup, it can support a later
saved change from `none` to `local`; availability itself launches no prompt.
The previous share is retired before the new revision can prompt or admit.
Without that backend, changing policy to local still refuses rather than
silently becoming unattended. Explicit process overrides preserve their existing
semantics and never bypass revision fencing.

## Verification boundary

The source includes protocol separation, original-capability, cancellation,
expiry, equal-ID replacement, native-subprocess ownership, monitored-session,
CLI-availability and fixed-bootstrap-budget regressions. The subprocess fixtures
exercise parent machinery, not physical-device positive consent or real tailnet
admission. The native X11 bridge has separate checks; those do not qualify this
complete Rust/host/client integration. Portable Mac policy and protocol-role
tests cover original gesture, source filtering, interrupted gestures, deadlines,
refusal, bounded disclosure parsing and fixed role/sequence binding. Tailnet
fixtures exercise the actual admission owner and the source of its presentation
metadata; they are not live Tailscale qualification. Apple-target Rust metadata
checks do not exercise AppKit, physical input or the window server. The existing
native audio/consent CI lane also compiles the Objective-C against the Apple SDK
and builds, links and runs strict Clippy on the actual consent helper.

Useful focused checks are also included in the existing CI lane:

```sh
cargo test -p fr-core indicator_process --locked
cargo test -p fr-tailnet --lib approval --locked
cargo test -p frd --lib desktop_approval --locked
cargo test -p frd --lib host_run::approval --locked
cargo test -p frd --lib host_run::policy --locked
cargo test -p frd --lib consent_budget --locked
cargo test -p frd --bin frd desktop_approval --locked
cargo test -p fr-native --no-default-features --features linux-input --lib approval_surface --locked
cargo test -p fr-native --no-default-features --features linux-input --bin fr-observation-indicator --locked
cargo test -p fr-native --no-default-features --lib macos_approval_policy --locked
cargo test -p fr-native --no-default-features --features macos-observation-indicator --bin fr-observation-indicator --locked
cargo test -p frd --features macos-host --lib service_install::options::macos_approval_tests --locked
```

Native mapping is not proof of physical visibility. The selected desktop user,
X server/window manager or macOS remain trusted. Real-device positive consent,
an installed monitored desktop and two-node tailnet acceptance remain
qualification requirements; the local-approval bead is not closed by source or
fixture tests.
