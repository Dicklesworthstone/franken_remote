# Local desktop approval in the Linux host

`frd run --approval local` uses the existing device-attributed X11 consent
surface before exposing a desktop. It distinguishes viewing from a session
requesting control. The result applies only to the original pending connection;
it is not approval of future peers, another share, or a reusable input lease.

## Build and select the local desktop

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

## Original lifetimes, not reusable approvals

The host notifies a capacity-one inbox with the ORIGINAL `Approval` capability,
then continues its existing admission and local-session servicing. Only the
original host's local-priority turn may consume a native decision. Stop, current
logind evidence and policy revision are checked before that turn; host startup
independently rechecks current tailnet admission and transport before granting
observation. Capture setup is invoked only after the approved session completes.

The native child reuses the existing XI2 source-device attribution and whole-
display input exclusion. XTest, forged core events and mapping alone cannot
supply a positive answer. A completed draw must precede an Allow gesture. Denial
or hiding in the same bounded event turn takes precedence. The child destroys
its prompt before returning a positive decision. The parent then reaps that
exact child and joins its worker before consuming the original capability.

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

Saved policy still controls each new share epoch. When the CLI has the matching
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
admission. The unchanged native C bridge has separate X11 checks; those do not
qualify this complete Rust/host/client integration.

Useful focused checks are also included in the existing CI lane:

```sh
cargo test -p fr-core indicator_process --locked
cargo test -p frd --lib desktop_approval --locked
cargo test -p frd --lib host_run::approval --locked
cargo test -p frd --lib host_run::policy --locked
cargo test -p frd --lib consent_budget --locked
cargo test -p frd --bin frd desktop_approval --locked
cargo test -p fr-native --no-default-features --features linux-input --lib approval_surface --locked
cargo test -p fr-native --no-default-features --features linux-input --bin fr-observation-indicator --locked
```

Native mapping is not proof of physical visibility. The selected desktop user,
X server and window manager remain trusted. Real-device positive consent, an
installed logind desktop and two-node tailnet acceptance remain qualification
requirements; the local-approval bead is not closed by source or fixture tests.
