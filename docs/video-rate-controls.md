# Local video frame-rate and bitrate controls

`frd run --fps N --bitrate BPS` configures the existing encoder and capture
path. Frame rate is an integer from 1 through 240; bitrate is an integer from
10,000 through 200,000,000 bits per second. Defaults remain 30 fps and
8,000,000 bits/s. Either option can be supplied independently. Invalid,
valueless, duplicate or overflowing values are refused, not clamped.
Numbers contain decimal digits only: use `12000000`, not `12M` or `12_000_000`.

For example, an explicit hardware attempt with a 60 fps maximum and 12 Mb/s
encoder target is:

```sh
frd run --encoder nvenc --fps 60 --bitrate 12000000 --approval none \
  --display :0 --worker /absolute/path/fr-media-worker
```

Use `--encoder vaapi` for the existing VAAPI path, or the explicit software
selector for CPU encoding. A lower-rate configuration is, for example,
`--encoder software --fps 15 --bitrate 2000000`. These examples select limits;
they are not qualified performance profiles or bandwidth measurements.
A lower target can trade picture quality for fewer encoded bytes. Actual
output depends on the selected encoder, content, source and transport.

The same settings pass through observation, controlled capture and the optional
observation-indicator wrapper. They remain fixed for the host run and its
existing bounded retries. No change is made to the saved authority policy.
Selecting rates does not select an encoder, enable audio/control/clipboard/files,
change sharing scope, or bypass local approval. All existing startup checks and
hardware failure behavior remain in place; hardware never falls back silently.

## Installed services

Pass rates with the existing run-argument boundary. The actual host parser
validates them before installation, and systemd preserves the selected values:

```sh
frd install --user --dry-run --approval none -- \
  --encoder nvenc --fps 60 --bitrate 12000000 --display :0 \
  --worker /absolute/path/fr-media-worker
```

The legacy installer software selector also works with carried rates:

```sh
frd install --user --dry-run --approval none --software-explicit -- \
  --fps 15 --bitrate 2000000 --display :0
```

A preview is not service activation or GPU probing. Existing platform, headless,
local-approval and explicit optional-capability restrictions still apply.

## Capture scheduling

The host no longer forces a minimum 50 ms interval on every encoder. It uses
one configured frame period rounded UP to microseconds: 33,334 us for 30 fps,
16,667 us for 60 fps, and 8,334 us for 120 fps. These are admission ceilings,
not measured or guaranteed delivered frame rates.

The shared capture source schedules from the actual admitted turn, rather than
adding a full period after capture, encoding, report and cursor processing.
Work that fits inside a period consumes that period. Late wakes re-anchor to
actual admission. A long operation skips elapsed opportunities in constant time;
exactly expired slots are skipped too. There is no accumulated catch-up queue.
Source/recipient expiry, earlier media deadlines and bounded maintenance remain
independent of this cadence. Idle source verification and change detection are
not replaced with continuous encoding or fabricated freshness.

The default may now admit more CPU work than the old hidden 20 fps cap. Set
`--fps 20` explicitly to select that maximum. Overload still uses the existing
bounded pool, backpressure and freshness checks rather than retaining stale
capture jobs. Existing input lease and usable-view deadlines are NOT extended
for low frame rates; selecting a low rate does not guarantee continuous control.

## Reporting and evidence

Listening JSON reports `video_fps_limit` and `encoder_bitrate_target_bps` as
configuration alongside `encoder_selection`. The bitrate is a codec target,
not an instantaneous network ceiling. This change is not automatic bandwidth
adaptation, quality-controller integration, zero-copy capture or GPU qualification.
CPU-staged X11 remains the capture path. The automatic rate-controller bead
remains open.

Deterministic tests cover every supported frame period, malformed/duplicate
flags, worker configuration roundtrips, healthy cadence, stalls, late wakeups,
clock regression and arithmetic overflow. Service tests feed generated ExecStart
arguments through the real parser. Real-frd preflight tests assert that rate
options remain subordinate to installed Tailscale and local approval, make no
connection to the fixture socket, and leave saved policy bytes unchanged.
These are configuration/scheduling checks, not measured native throughput.

```sh
cargo test -p frd --lib video --locked
cargo test -p frd --lib shared_publisher::service --locked
cargo test -p frd --lib host_run::tests --locked
cargo test -p frd --test encoder_cli --locked
```

## Bounded software bitrate changes in the native worker

The Linux software capture worker accepts an explicit target on one original
capture credit and reports the **actual applied bitrate** with its encoded or
unchanged result. A request alone does not mean the encoder changed. This private
worker operation is available to the media controller; ordinary fixed-rate
capture keeps its existing request/reply contract.

A lower target waits for a real changed, unconditional or recovery capture.
An idle observation or optional settle-only refinement does not restart the
encoder. After a validated transition, both the original host timestamp and the
worker's monotonic clock must advance two seconds before another transition.
The worker accepts targets only between the original ceiling and the greater of
10,000 bits/s or one sixteenth of that ceiling. Hardware returns a typed
unsupported result; it never falls back to software.

The software adapter stops its one old encoder before reopening libx265. It
retains the original frame identities, geometry, color, GOP, nominal frame rate
and decoder-configuration admission guard, and pins the HEVC level learned from
validated original output. The first new picture must be an IDR with byte-exact
original VPS/SPS/PPS. A reopen or parameter-set failure ends that source instead
of releasing incompatible media. There is no overlapping encoder or extra
capture queue. Legacy recovery capture cancels a still-deferred rate request so
an old response shape cannot conceal an applied change.

Production-source tests with FFmpeg 6.1/libx265 3.5 and Xvfb passed: 46 native unit
tests (one pre-existing manual benchmark ignored), all 13 capture regressions,
all five new worker-rate protocol tests and all nine existing worker protocol
tests. These include actual stop/reopen, original-decoder playback, malformed
requests, pending-output refusal, idle deferral, dual-clock dwell and a spawned
worker exchanging actual-rate responses. The native suite adds eight tests;
the capture suite adds three.

In the deterministic 640x480 mixed text/busy fixture, an 8,000,000 to 2,000,000
bits/s change reduced the recovery IDR from 72,937 to 29,195 bytes and 60 nominal
30 fps pictures from 1,982,529 to 505,800 encoded bytes. The original decoder
accepted every picture with its original configuration and timestamps. These
are encoded-payload measurements, not elapsed delivery or network throughput;
reduced bitrate can reduce picture quality. They do not qualify hardware,
resolution changes, or live impaired-path recovery. Full workspace/Cargo and
daemon verification remain blocked by the shared host's exhausted disk/memory
and incomplete pinned runtime dependency checkout.
