# Explicit HEVC encoder selection in the Linux host

`frd run --encoder nvenc` and `frd run --encoder vaapi` select the existing
supervised worker's hardware encoder paths. `--encoder software` selects the
CPU developer profile, equivalent to the existing `--software-explicit` flag.
These are explicit local choices, not negotiated peer values or arbitrary
FFmpeg codec names. No automatic selection or fallback is performed.

The same selection reaches both observation and controlled capture. It remains
fixed across new shares, saved-policy revisions and the existing bounded restart
policy. Enabling the optional observation indicator preserves it as well. Legacy
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

Use `--encoder vaapi` for the existing VAAPI path. Use `--encoder software` or
`--software-explicit` for the software profile, but never both. The installed
Tailscale, certificate, selected local desktop and ingress-helper/root
requirements are unchanged. For control, add the existing `--input-agent` option;
audio, clipboard, file reception and session monitoring remain separate enables.
The view-only observation indicator can be selected independently, subject to
its existing restriction against combining it with `--input-agent`.

Unknown names, duplicate selectors and mixing `--encoder` with
`--software-explicit` are refused before configuration/network work. No selector
still refuses: the legacy `hardware_hevc_unavailable` code is retained for that
case, with an explanation that no automatic encoder selection exists. The
listening JSON adds `encoder_selection`, which names the operator's choice,
not successful hardware initialization or qualification.

## Installed service configuration

Pass the explicit encoder through the installer's existing `--` boundary:

```sh
frd install --user --dry-run --approval none -- \
  --encoder nvenc --display :0 --worker /absolute/path/fr-media-worker
```

The generated systemd command preserves exactly that selection, without adding
`--software-explicit`. Do not combine the legacy installer software flag with an
encoder in the carried run arguments, even `--encoder software`: the contradictory
unit is refused. An omitted encoder still refuses. Existing service constraints
remain, including separate control/clipboard/file enables, the local-approval
refusal and the requirement for headless mode when carrying run options into a
system-wide unit. Other platforms do not silently discard the hardware flags.

This preview renders configuration; it does not start a service, probe a GPU,
change saved admission policy or qualify the selected encoder. The service
account still needs the existing local desktop, device and ingress permissions.

## What a hardware selection proves

By itself, nothing about the hardware. Native codec initialization still runs
in the capture worker after admission and media negotiation. The selected
encoder must actually open, its output must pass the existing HEVC parameter-set
and reference checks, and decoder startup must complete. An unavailable device,
driver or incompatible bitstream follows the existing typed worker/share
failure path. It never retries that work with software or another backend.
The existing host restart budget and backoff are unchanged.

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
latency or CPU-utilization claims. `--approval local` remains unavailable;
no approval or admission check is relaxed.

## Validation and remaining qualification

The selection regressions exercise exact parsing, contradictory flags, display
selection, private worker configuration roundtrip and the existing codec bounds.
They also check that selecting hardware cannot enable control, audio, clipboard,
files or change approval/sharing policy. Service tests roundtrip the rendered
systemd arguments through the real host parser. The real-binary preflight tests
stop before native work, assert that local approval and missing Tailscale still
refuse, and verify the saved policy remains unchanged.

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
