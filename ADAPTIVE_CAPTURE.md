# Adaptive raw-capture admission

This is an implemented, opt-in local pacing slice of plan section 13 and bead
`fr-p1-adaptive-v0-qgi`. It runs in the existing continuous host session and uses
its real source and QUIC sender. It is **not** a complete adaptive bitrate,
resolution, end-to-end latency, or per-path aggregate-bandwidth controller.

## Local host selection

The shipped Linux host exposes pacing for both an observation-only shared source
and the first controlling viewer:

```sh
frd run --encoder software --adaptive-capture
# Add --input-agent /usr/libexec/fr-input-agent to offer control as well.
```

The option is off by default. It is a local capture-policy choice, not a
negotiated peer capability. The existing observation approval and source
permission checks still govern the share; control additionally requires its
input permission, indicator and lease. Installed services preserve the choice
through their `--` arguments, including on hosts that do not offer control.

The original controlled publisher receives the existing measured source-work,
send-admission, retention-credit and negotiated decoder-load samples. Collected
native input retains its existing bounded idle wake path. The encoder bitrate,
resolution and configuration do not change. An observer-first share now uses the
same pacing state machine in its original shared publisher, with measured source
work, genuine changed/unchanged observations and aggregate capture credit. A
healthy eligible subscriber keeps the source eligible while a lagging subscriber
retains its own existing expiry and retirement rules. Actual video enqueue
samples also contribute: any recently ready streaming subscriber establishes
local send readiness, while shared send pressure requires every eligible
subscriber to have recent blocked evidence. Startup, recovery and failed
subscribers do not vote; silent or stale samples remain unknown. Decoder and
delivery measurements remain unknown for this shared-source controller, and
neither enqueue success nor capture credit is treated as a bandwidth estimate.

The `--fps` setting remains the maximum capture rate. The adaptive maximum is
200 ms unless the configured capture interval is already slower; a `--fps` value
below 5 then keeps that slower interval fixed. The host never speeds up a low-fps
operator choice to fit the adaptive range.

A dispatch regression follows the actual controlling owner through native
UDP/TLS/QUIC and supervised capture, presentation and input IPC. It checks a
genuine unchanged source reaches verified idle, the original input receipt and
ordered native cleanup still complete, and pacing disappears with the reaped
publisher. Codec output, native input effects and visible confirmation in that
regression are explicit protocol fixtures, not hardware or physical presentation
evidence. Separate parser, service-render, profile and low-fps-policy regressions
cover the selection boundary.

The shared-dispatch regression also follows first-viewer consent, the original
capture child, source renewal and a later viewer through actual UDP/TLS/QUIC and
supervised protocol-fixture IPC. The later viewer continues after the first
connection departs, with pacing owned by the source rather than either viewer.
This remains fixture-based integration evidence, not live-tailnet qualification.

## Enabling the implemented path

Call `Stream::enable_adaptive_capture` or `StreamingHost::enable_adaptive_capture`
on an already admitted native stream, before starting `serve`:

```rust
host.enable_adaptive_capture(std::time::Duration::from_millis(200))?;
```

The minimum interval is the stream's existing, encoder-qualified capture policy,
rounded up to a whole microsecond. The maximum is caller-selected and cannot
exceed 200 ms, except that an already slower fixed interval may be retained as
both minimum and maximum (up to one second). Invalid limits, repeated configuration, and a stopped or already
serving owner are refused without replacing the controller. Existing callers
retain their fixed policy until they explicitly enable adaptation.

For an independently owned shared source, call
`Publisher::enable_adaptive_capture(capture_interval, maximum)` before its
continuous service. The dispatch driver exposes the same selection through
`Driver::with_shared_adaptive_capture(maximum)` and reports the original source's
controller through `Driver::capture_pacing()`. The minimum interval must match
the subsequent service cadence and the encoder's frame-rate ceiling.
Shared cursor sampling keeps its original cadence when adaptive video slows;
pointer motion does not create a video frame or source-freshness evidence.

The pure state machine is `fr_media::pacing::Controller`. It keeps fixed-size
state and the last sixteen adjustments, with no heap allocation or media bytes.
`StreamingHost::pacing()` exposes that controller's reports after service. Input
samples, elapsed dwell evidence, prior/new intervals, and reason codes are
content-free and replay deterministically; a decision history is a bounded
suffix, not an unlimited recording of every sample.

## Real evidence, separate causes

The host samples these existing owners on its original monotonic clock:

| Input | What it actually establishes |
| --- | --- |
| Source-work duration | Time from one issued capture credit through queueing, native work, and result collection; not an isolated encoder-time measurement. A live operation's increasing age is included. A completed duration is retained for at most 250 ms. |
| Send admission | Whether an actual `QuicEgress::transmit` attempt was backpressured. Enqueue acceptance is not delivery, link capacity, congestion-window measurement, or proof of client presentation. |
| Capture credit | Whether the existing sender can retain another maximal result while preserving its original records and reference cache. Retention pressure is not labelled network congestion. |
| Source observation | The original timestamp and changed/unchanged result from the supervised conditional capture. No observation is constructed from polling, transport traffic, absence of pictures, or receipt time. |

Unavailable measurements remain unknown. There is no invented decoder, GPU,
presentation, delivery-rate, or one-way network measurement in this controller.
Its upward probes establish **local** headroom only and never exceed the already
configured capture-rate ceiling. Receiver, freshness, transport, and authority
limits remain independent enforcement boundaries.

## Adjustment rules

Startup uses twice the minimum interval, clamped to the maximum. A continuously
observed overload must persist for 100 ms before reducing offered raw work; each
reduction doubles the interval within its bounds and is separated from the next
by at least 250 ms. Source-work, send-admission, and capture-credit causes retain
distinct reason codes.

Upward probes require two seconds of measured local headroom, a recent genuine
source observation, known ready sender credit, and no send backpressure. Each
probe reduces the interval by at most one eighth, rounded upward in microseconds,
without passing the configured floor. A source duration above three quarters of
the interval cannot certify headroom. Only intervals whose endpoints both have known headroom count. Brief unknown
intervals pause the evidence clock; known pressure or more than 100 ms without
known evidence resets it. Unknown evidence never triggers or contributes time
to a probe. This preserves the concurrently landed `b82f1e4` correction.

Idle is established only by distinct, recent unchanged observations spanning at
least one second. It moves to the bounded maximum interval and keeps genuine
source checks and observation renewal running without dummy encoded frames.
The first actual changed observation exits idle at the conservative starting
interval, unless current measured pressure forbids speeding up. Until the next
source check, change detection remains polling-limited by the selected interval;
platform damage-event notification remains outside this controller. The separate
[collected native-input wake path](INPUT_CAPTURE_WAKE.md) can advance one bounded
source check during verified idle without altering these pacing decisions.

A sampling gap longer than 100 ms resets accumulated dwell and idle evidence.
Missing source evidence removes the idle classification without manufacturing a
changed frame. Clock regression, future source timestamps, and contradictory or
regressing source metadata are terminal typed controller errors.

## Ownership, deadlines, and cancellation

Only the **next raw admission** is rescheduled. The existing one-credit discipline
still covers the sole queued, executing, or uncollected capture result. An
already-issued operation is neither cancelled nor duplicated by a pacing change.
Missed capture opportunities are not stored or replayed as a catch-up burst.

The native source, codec configuration, reference chain, capture timestamps,
packet bytes, pending send expiry, repair spending, receiver budgets, input
identities, and authority deadlines remain unchanged. A slower interval is not
permission to retain an expired picture or action. Independent native-input
cleanup and session renewal continue through the same host driver.

## Verification boundaries

Twelve pure tests cover pressure attribution, asymmetric step/dwell behavior,
unknown evidence, genuine idle and wake, stale or replayed observations, clock
faults, arithmetic limits, exact replay, and fixed decision-history retention.
Five host integrations use production startup and actual UDP/TLS/QUIC plus
supervised subprocess IPC with an explicitly synthetic codec. They exercise
idle across lease renewal, change after idle, slow source work, actual exhausted
reference-retention credit, and configuration/lifecycle refusal.

The explicit `actual_hevc_streams_adaptive_idle_wakes_without_replacing_capture_or_decoder`
test uses the real FFmpeg worker and two private X11 displays. It checks four
changed pictures by pixel readback across renewals and verified idle, no extra
encoded pictures, conservative wake, fewer conditional captures, and unchanged
native capture/decoder process identities. This is software HEVC and X11 readback,
not hardware acceleration, physical scanout, or live-tailnet ingress proof.

The base revision retains the separately published `17854a7` correction to
`Presenter::present_next`'s immediate receiver-close contract on abandoned
borrowed decoding. This increment does not replace that correction or its
queued-dependent regression. The detached continuous path remains independent.

The bitrate is fixed for a session, but the software encoder's **short-term**
rate is bounded: libx265 runs with a VBV at the configured bitrate and a 200 ms
buffer (`crates/fr-native/src/bridge.c`). Without it, libx265's average-rate
control emitted single P pictures of up to 77 KB at a 2 Mbit/s target on
incompressible content: about 300 ms of link time at that rate, past the 250 ms
reference horizon. With it the largest P picture was about 17 KB and the IDR
about half its former size. The namespace row
`real_impairment::view_only_incompressible_content_over_a_narrow_link` (a noise
window, the test host's 2 Mbit/s target) holds every change on a 3 Mbit/s link,
where the session used to end within seconds (0/6 shown; also 1/6 at 4 Mbit/s).
NVENC gets the same bound. On a GTX 1070 (640x480, 15 fps, 2 Mbit/s, partial
noise) its defaults emitted P pictures of up to 70 KB; with the bound the largest
was 24 KB and the IDR fell from 85 to 69 KB. VA-API keeps its libavcodec defaults
(not measured).

Remaining work includes qualified runtime bitrate/quantizer/resolution changes,
remote decoder and presentation feedback, aggregate host/path budgets, qualified
damage-event and local-input wakeup, and platform/GUI policy selection. Collected
remote-input wakeup is implemented separately in
[INPUT_CAPTURE_WAKE.md](INPUT_CAPTURE_WAKE.md). The broad adaptive-control bead
remains incomplete.

## Direct publication verification

The recovered source increments were reconciled with `b82f1e4`, preserving its
canonical controller and three additional regressions. Fresh local verification
passed 421 core/wire/media/client Cargo tests in an isolated workspace, 80 rebuilt
daemon tests, and all six explicitly executed native HEVC/X11 streaming tests.
The 80-test run reports six native cases ignored; the separate explicit run
executes all six, with zero failures or remaining ignored cases in that lane.

The runtime checks rebuild first-party sources with the exact pinned compiler
against retained matching dependencies. The native worker is rebuilt from the
current Rust and C sources against the installed FFmpeg/X11 libraries. These
results are not a fresh full-workspace Cargo dependency build or remote CI pass.
Repository formatting and documentation-link checks are separate validations.

## Negotiated decoder-load integration

[Continuous receiver feedback](RECEIVER_FEEDBACK.md) joins the published
`decoder-metrics` query/reply protocol to actual continuous host/viewer service.
The original query deadline bounds sample age, and measured receiver backlog and
decode-service time inform the canonical controller without overriding its local
rate, deadline or ownership constraints. Platform presentation measurements,
runtime codec reconfiguration and aggregate bandwidth control remain separate.
