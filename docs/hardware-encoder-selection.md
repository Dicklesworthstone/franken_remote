# HEVC encoder selection in the Linux host

`frd run --encoder nvenc` and `frd run --encoder vaapi` select the existing
supervised worker's hardware encoder paths. `--encoder software` selects the
CPU developer profile, equivalent to the existing `--software-explicit` flag.
These are explicit local choices, not negotiated peer values or arbitrary
FFmpeg codec names. `--encoder auto` explicitly asks the host to probe NVENC,
then VAAPI, selecting the first backend that captures this X11 display and
produces an admitted HEVC IDR. It never tries software. An exact `nvenc` or
`vaapi` request still attempts only that backend.

The same concrete backend reaches both observation and controlled capture. After
startup selection it remains fixed across new shares, saved-policy revisions
and the existing bounded restart policy. Auto is resolved once before serving,
never again because a share fails or a device disappears. Enabling the optional
observation indicator preserves it as well. Legacy
library entry points retain their explicit software behavior; callers selecting
hardware use `host_run::run_with_policy_and_encoder` or the corresponding
`host_indicator` entry point.

## Foreground host

Build the same capture worker and daemon as before, using the repository's
pinned toolchain and native SDK prerequisites:

```sh
cargo build -p fr-native --features linux-displays --bin fr-media-worker --locked
cargo build -p frd --bin frd --locked
```

Select one backend when starting the host:

```sh
frd run --encoder nvenc --approval none \
  --display :0 --worker /absolute/path/fr-media-worker
```

Use `--encoder auto` to test the two available hardware paths without knowing
which is usable, or `--encoder vaapi` to require the existing VAAPI path.
Auto uses a fixed NVENC-then-VAAPI order; it does not measure which device is
fastest or select a particular adapter on a hybrid-GPU system.
Use `--encoder software` or
`--software-explicit` for the software profile, but never both. The installed
Tailscale, certificate, selected local desktop and ingress-helper/root
requirements are unchanged. For control, add the existing `--input-agent` option;
audio, clipboard, file reception and session monitoring remain separate enables.
The view-only observation indicator can be selected independently, subject to
its existing restriction against combining it with `--input-agent`.

Unknown names, duplicate selectors and mixing `--encoder` with
`--software-explicit` are refused before configuration/network work. No selector
still refuses: the legacy `hardware_hevc_unavailable` code is retained for that
case, with instructions to choose a selection explicitly. The listening JSON
contains `encoder_selection`, which names the operator's request. For auto, an
earlier `encoder_selected` event names the concrete `encoder` and reports
`startup_idr_probe: "passed"` and `hardware_qualification: "not_tested"`.
The human output likewise names the winner. A successful probe is not a
hardware qualification or a promise of future device availability.

## Installed service configuration

Pass the explicit encoder through the installer's existing `--` boundary:

```sh
frd install --user --dry-run --approval none -- \
  --encoder nvenc --display :0 --worker /absolute/path/fr-media-worker
```

The generated systemd command preserves exactly that selection, including
`--encoder auto`, without adding `--software-explicit`. Automatic selection is
performed by the running service on its actual desktop, never by the installer.
Do not combine the legacy installer software flag with an
encoder in the carried run arguments, even `--encoder software`: the contradictory
unit is refused. An omitted encoder still refuses. Existing service constraints
remain, including separate control/clipboard/file enables, local-approval
prerequisites and the requirement for headless mode when carrying run options into a
system-wide unit. Other platforms do not silently discard the hardware flags.

This preview renders configuration; it does not start a service, probe a GPU
(the running service probes at its own startup),
change saved admission policy or qualify the selected encoder. The service
account still needs the existing local desktop, device and ingress permissions.

On Linux, installation writes a private, exclusively created staging file and
publishes the complete service definition atomically. It never reuses an old
`frd.tmp`; symlinks, multiply linked units, nonregular files and untrusted writable
service directories are refused. Every directory component is opened without
following links, and publication/removal uses the original directory handle.
Concurrent installers have separate stages; the last complete publication wins.
Uninstall uses the same path checks, leaves missing directories absent and does
not stop an already running service.

Install and uninstall JSON include `durable`: `null` for a preview or when no
removal occurred, `true` after synchronizing the changed directory, and `false`
if publication/removal completed but directory synchronization failed. That last
result retains the completed filesystem change and reports the uncertainty in
`next_steps`; it does not claim that nothing happened. The selected local user
and root remain trusted. These checks do not qualify a service manager, GPU or
live remote session, and persistent installation remains Linux-only.

The generated root ingress-helper unit uses the canonical executable whose
file and ancestor ownership were checked, so retargeting the installer's original
symlink cannot redirect the root service. A protected directory or nonexecutable
file does not qualify as that executable. When a protected copy is needed, the
reported administrator command quotes each path as one literal shell argument;
installation still does not execute that command or start the helper.

## What a hardware selection proves

`frd run --encoder nvenc|vaapi` probes the selection once at startup, before any
certificate, listener or firewall rule exists. The capture worker
(`fr-media-worker --probe-encoder BACKEND`) captures the X root window, opens
the selected encoder at that size, encodes one forced IDR and passes it through
the same encoder-SEI stripping and HEVC parameter-set/reference admission as a
share. An encoder that cannot open, an inadmissible first IDR, or no reply
within 15 seconds refuses the run with `encoder_unavailable` (the worker's
typed reason or timeout cause is in the detail). Before this, an unusable GPU
encoder surfaced only when the first viewer's share failed. The explicit
software profile is not probed. An exact hardware request never substitutes
another backend.

For `--encoder auto`, only a completed, reaped refusal permits the next hardware
candidate. Each child has the same 15-second execution deadline, with at most
two attempts. A timeout, launch failure, invalid reply or uncollected child stops
the selection instead of starting another probe. Reply dimensions must also fit
the current protocol limits. A local Stop prevents any subsequent probe and
suppresses startup after a cleanly collected result; cleanup failures remain
failures even when Stop was requested. If both backends refuse, the run reports
`encoder_unavailable` with both bounded, content-free reasons. No certificate,
listener or firewall rule is created by an unsuccessful selection, and no CPU
encoder is selected implicitly.

The probe proves that encoder can produce admitted HEVC on this display at
startup, not that every later share will. Native codec initialization still
runs again in each share's capture worker after admission and media
negotiation, and decoder startup must still complete. A later device, driver or
bitstream failure follows the existing typed worker/share failure path. The
existing host restart budget and backoff are unchanged.

The current adapters remain **CPU-staged X11 capture**. NVENC/VAAPI selection
changes encoding, not capture ownership, zero-copy behavior, input authority,
client decoder implementation or the transport. The native bridge's existing
default-device selection remains; this change adds no GPU index or render-node
selector. It adds no codec, runtime, native library or Rust dependency.

The defaults remain 30 fps and an 8,000,000 bits/s encoder target. Local
`--fps` and `--bitrate` controls now configure both the codec and the host's
admission cadence, without the former fixed 20-capture-per-second floor.
See [video rate controls](video-rate-controls.md) for bounds, service use and
no-catch-up scheduling. These settings are not measured throughput, quality,
latency or CPU-utilization claims. Local approval retains its required desktop
consent and session monitor; no approval or admission check is relaxed.

## Validation and remaining qualification

The selection regressions exercise exact and automatic requests, first and
second candidate success, both refusals, fatal probe/cleanup failure, no software
fallback, contradictory flags, display selection, private worker configuration
roundtrip and the existing codec bounds.
They also check that selecting hardware cannot enable control, audio, clipboard,
files or change approval/sharing policy. Service tests roundtrip the rendered
systemd arguments through the real host parser. The real-binary preflight tests
stop before native work, assert that incomplete local-approval configuration and
missing Tailscale still refuse, and verify the saved policy remains unchanged.

```sh
cargo test -p frd --lib encoder --locked
cargo test -p frd --lib service_install --locked
cargo test -p frd --test encoder_cli --locked
cargo check -p frd --bin frd --locked
```

For a locally provisioned GPU/X11 test machine, the existing native roundtrip
example can exercise the actual chosen native encoder, admitted HEVC, decode
and X11 readback:

```sh
cargo run -p fr-native --features linux-media --example native_roundtrip -- --nvenc
# Or use --vaapi; software requires --software-explicit explicitly.
```

That example is native library/X11 evidence, not a networked `frd run` session,
physical visibility or a platform-wide capability claim. GPU execution,
controlled-session behavior on that hardware, performance measurements and a
real two-node tailnet session still require revision-bound qualification.
A passing parser or CPU CI run is not a passing GPU row.
