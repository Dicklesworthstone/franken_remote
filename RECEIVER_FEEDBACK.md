# Continuous receiver-load feedback

This implements runtime integration for the existing `decoder-metrics` version 1
protocol and plan section 13. The continuous viewer measures real accepted decode
obligations and charged receiver buffers. Its original host feeds the solicited
reports into the canonical capture-pacing controller. The broad adaptive-media
bead remains open: this is not bitrate/resolution reconfiguration, physical
presentation measurement, aggregate budgeting or additional authority.

## One canonical protocol

Both offers must positively select `fr_wire::receiver_metrics::CAPABILITY` at
version 1. The existing `StageMetrics` kind carries a 130-byte query and a 152-byte
reply, using the published codecs and `fr_media::receiver_feedback::{Requester,
Responder}`. No parallel wire format, new stream, nonce authority, runtime or
unrequested fallback is introduced. Unnegotiated viewers send no reports;
unsolicited metrics and mismatched directions, routes or full view identities
refuse. Both the original session-control binding and the full decoder view must
match. The selected control-record limit must admit the full reply at attachment.

The host's existing `enable_adaptive_capture` must be enabled before `serve` to
change capture pacing. A fixed-policy host can consume negotiated measurements
without silently enabling adaptation. Existing local-only users of the capture
controller retain their previous behavior.

`frd run` enables adaptive capture only with the explicit `--adaptive-capture`
option; it remains off by default for controlled and shared observation sessions.
Measured 2026-10-14 (namespace, controlled share,
the client pinned to one core shared with CPU hogs, busy host content, 60 s,
debug builds):
- 3 hogs: both survived. With pacing: 461 pictures instead of 901, presented
  age p95 120 ms instead of 100 ms, and 8 input suspensions (9.0 s) instead of
  1 (1.2 s). Fewer pictures leave the presented view older, so input's view
  freshness gate suspends control more often.
- 8 hogs: both ended, with pacing sooner (14 s `view_stale` vs 27 s
  `host_session_ended`).

So for controlled shares, slowing capture does not protect an overloaded client;
it trades frame rate for input suspensions.

The previously published wire grammar, validation, `Requester`, `Responder` and
`ReceiverEvidence` retain their existing sequence and lifetime rules. The runtime does not alter their
original 50 ms host query cadence, 150 ms query/sample lifetime, 100 ms reply
lifetime or 25 ms minimum reply sampling interval. Sample validity begins when
the host creates the query, not when either side queues or receives its record.
Late, duplicate and unsolicited replies cannot establish fresh evidence. No
comparison of clocks on different machines is required.

## Real measured work

`ViewerFeedback` measures from accepting a `DecodeJob` until the original
receiver acknowledges its actual native completion. While native work is pending,
it reports the increasing duration of that obligation. The duration includes IPC,
native execution and uncollected completion; it is not isolated codec CPU/GPU
execution time. Compressed counts and bytes come from the receiver's actual
charged budget, including incomplete pictures, metadata and decoder-owned bytes.

The maximum of active work age and a recently completed duration is reported. A
newly started job therefore cannot hide a slow previous decode with a near-zero
age. Completed-duration evidence expires after 250 ms of viewer-local time.
`decoding` names an actual accepted obligation, and missing work evidence remains
unknown instead of being encoded as zero. Neither successful decode nor a metrics
reply creates a visibility callback, source-freshness observation or input grant.

## Scheduling and bounds

Host queries and viewer replies use the existing session-control pair and final
transport admission checks. Each side retains at most one original immutable
pending record, using the existing pure request/reply owners. Backpressure does
not replace its bytes, sequence, sample or deadline. Only an expired unadmitted
record can be retired; an admitted reliable record retains its transport lifetime.

Metrics processing remains outside the native input mailbox. Canonical renewal,
input receipts, repair and connection service continue while the supervised
decoder waits. The continuous viewer still finishes healthy network turns rather
than abandoning their futures when decode completes. Constructors reject foreign
connection/view joins, and failure fences control before native cleanup.

The canonical controller distinguishes receiver backlog from decoder-service
pressure. Sustained pressure reduces future raw admission within the existing
step, dwell and configured rate bounds. A negotiated peer without a timely known
sample cannot justify a headroom probe. In-flight decode likewise cannot certify
spare capacity. Existing source-idle and conservative wake rules remain intact.
Only the next raw capture is rescheduled: encoded references, original media and
input deadlines, held-state ownership, action identities and current native jobs
are unchanged.

### Pressure evidence across reply gaps

A single outstanding query with a 150 ms lifetime has only about 30 ms of usable
reply evidence on a 120 ms RTT path. Resetting the overload clock on every
unknown interval made the 100 ms pressure dwell impossible to complete, even
with repeated timely reports of overload.

Receiver backlog and decoder-work dwell now accumulate only intervals whose two
observed endpoints both establish that cause. An unknown sample reports zero
active pressure, contributes no elapsed time, and breaks interval continuity.
It can preserve earlier measured intervals for at most 100 ms since the last
positive sample. The first positive sample after that gap contributes no gap
time. Known nonpressure, explicit absence of negotiation, or a longer gap
clears the history. The next reduction still requires a current positive report
and the original 250 ms adjustment cooldown; local pressure retains precedence.

The requester, responder, one-record bounds and original query expiry are
unchanged. A reply arriving at or beyond 150 ms is still expired. This correction
allows multiple brief **timely** reply windows to establish measured overload;
it does not claim headroom from silence or qualify arbitrary RTTs.

Private recovery clears receiver-pressure and receiver-dependent headroom
evidence when it installs a new feedback generation. Shared pacing compares the
full bindings in its fixed eight-slot eligible-viewer inventory at both actual
sampling sites, under the existing members lock. Membership, eligibility and
generation changes clear that evidence without resetting local work/send/credit
history, the interval or adjustment cooldown.

For shared sources, known nonpressured or unnegotiated evidence from an eligible
viewer also clears aggregate receiver pressure. This applies even if that
viewer's send path is blocked. It supplies no synthetic headroom: upward probes
still need one same-viewer pair of accepted enqueue and healthy decoder evidence.

`StreamingHost::receiver_feedback_reports` counts accepted new solicited reports.
`StreamingViewer::receiver_feedback_reports` counts reply admissions into transport.
Neither is an acknowledgement of delivery, presentation or application effects.

## Shared observation sources

Each original shared session still owns its requester, immutable pending query,
transport routes and reply validation. After its startup handshake, it passes
one bounded, content-free sample to its subscriber on the original connection.
The publisher owns no requester, connection or additional queue. The snapshot
contains the exact installed media view and the original query expiry; reading
or copying it cannot create a new lifetime. The control-pair binding used on the
wire and the media-configuration binding remain separate, validated scopes.

The shared controller considers only active streaming members. Startup, late
join, recovery and failed members do not vote. Recovery changes the complete
media binding, immediately invalidating an old sample even before a replacement
report arrives. Missing, stale or future evidence is unknown. Only the original
session can explicitly record that metrics were not negotiated; an uninitialized
member cannot claim that exemption.

Upward probes require a single eligible viewer with both recent accepted video
enqueue and current receiver headroom. A blocked sender's healthy decoder cannot
combine with a different viewer's slow decoder and ready sender. Receiver-driven
slowdown requires every eligible member to report positive pressure within its
original lifetime. The representative is one complete measured report, never
componentwise minima assembled from different viewers. The existing source-work,
pool-credit and send-pressure causes remain independently authoritative.

Recent enqueue evidence is bounded by the current capture interval, with a
100 ms minimum and 250 ms maximum. This permits recovery from a 200 ms cadence
without requiring an enqueue that the cadence itself forbids. Its timestamp is
the actual enqueue attempt; idle maintenance does not refresh it. This does not
extend the independent 150 ms query/sample deadline or prove network capacity.

Two shared-source regressions use the original production feedback owners and
real UDP/TLS plus supervised source IPC. One delays an actual accepted receiver
obligation's fixture completion, observes raw-capture reduction, then resumes
prompt completion and requires an upward probe across session renewal. The other
keeps a healthy sibling progressing through the slow viewer's departure. Codec
output and decode completion are explicit fixtures; no hardware or WAN result
is implied. Pure regressions separately cover sample expiry, binding changes,
mixed-viewer evidence, missing reports and cadence-limited probe recovery.

## Delayed-feedback validation, 28 October 2026

The exact production `fr-media` library and the complete pacing and
receiver-feedback test targets were rebuilt with the pinned compiler against
the original production core/wire libraries. **All 31 tests passed**: 15 pacing
tests and 16 receiver-feedback tests, including seven added cases.

The deterministic 120 ms RTT replay runs the actual `Requester`, `Responder`
and controller with independent host/viewer clock origins and the original
query deadlines. Before this correction, two seconds produced no reduction and
never accumulated more than 20 ms of live pressure. The corrected controller
first changes the capture interval from 40 ms to 80 ms at 620 ms, after exactly
100 ms of positive endpoint intervals across five reply windows. Every unknown
window reports zero active pressure. A separate 160 ms RTT replay keeps all late
replies unknown and makes no receiver-driven adjustment.

The tests also cover isolated positive endpoints, the exact 100 ms pause bound,
known healthy/unnegotiated counterevidence, scope resets, old headroom removal,
preserved local-pressure precedence and the unchanged reduction cooldown.
Additional shared-owner regressions cover a healthy but blocked sibling,
membership/generation resets and continued same-view headroom. Those daemon
regressions are source-reviewed but have not been executed in this session.
This is deterministic protocol/controller evidence, not live WAN impairment,
physical presentation or a full-workspace Cargo pass.

## Verification scope

Eight added runtime tests cover exact negotiated attachment, original routes and
full views, the immutable backpressured reply, recent slow-decode evidence, real
QUIC send backpressure, absent negotiation and two continuous network integrations.

The sustained test uses actual localhost UDP/TLS, production session negotiation,
media attachments and supervised **synthetic** codec subprocesses. A 90 ms decoder
wait causes the host to reduce actual capture admissions while network service
and observation renewal continue beyond the initial three-second lease. A second
integration exercises real input records and returned native-owner receipts during
the decoder wait; its native input sink is counted, not X11. Removing receiver
pressure from the host controller causes the unchanged sustained regression to
fail through an early decoder deadline, rather than simply changing a reported
statistic. A separate negative copy that retimes samples from reply receipt fails
the existing canonical absolute-expiry regression.

The additional explicit native regression,
`actual_hevc_streams_receiver_feedback_slows_capture_with_real_decoder_and_pixels`,
uses the real FFmpeg worker, two private X11 displays, and actual QUIC/UDP/TLS.
A supervised request proxy adds 90 ms before real decoder execution; it never
substitutes decoded data or completion receipts. The test verifies each completed
picture by X11 pixel readback, sustained solicited feedback, a decoder-load pacing
reduction, continued session renewal beyond the original lease, and unchanged
capture/decoder process identities. All six pre-existing native streaming cases
remain intact.

## Publication verification, 12 September 2026

The recovered runtime was reconciled with the exact published decoder-preparation
revision and committed as `016aeb0`; the actual-codec regression is `ca9b8ac`.
An existing input-during-decode fixture occasionally assumed immediate send capacity after channel attachment. It now
retains the exact bytes and original deadline while servicing both endpoints,
retrying only `Backpressure` before admission. Already accepted records are not
replayed. No production deadline, assertion, or authorization check was relaxed.

Fresh local verification passed 434 shared-crate Cargo tests, 95 daemon tests,
40 native transport/attachment/lifetime tests, and seven explicitly executed
HEVC/X11 streaming tests: **576 selected passes, zero final failures**. The ordinary
daemon run excludes ten explicit cases; seven were executed in the native lane,
while the three isolated-network public-connection cases were not rerun locally.
All seven native streaming cases passed with one, four, and eight test threads.
The complete 95-test daemon suite and focused feedback/input regressions passed
the same thread-count matrix.
The original fixture failure and the native environment startup failure are
retained in the evidence rather than counted as passes.

Two separately broken copies fail unchanged regressions: ignoring receiver load
at the actual capture controller causes premature decoder/media expiry, and
replacing query-created expiry with reply-receipt time incorrectly preserves an
old measurement. The shipping code passes both cases.

Runtime-local builds freshly compile first-party sources with the pinned
compiler against matching retained dependencies. The native worker is rebuilt
from current Rust/C sources against matching FFmpeg/X11 SDKs and libraries.
Clean CI separately built the complete dependency graph: run `34694704856`
verified the exact runtime source, and run `34695031703` passed the additional
real-codec test and all seven explicit native streaming cases. Both runs completed
successfully. Each publication uses those exact verified source objects. The
finalized workflow checks committed sources read-only, without candidate patching
or Git-object writes. A fresh run of the final documentation/workflow checkout is
a separate verification, not automatically implied by these candidate passes.

The first local native attempt exhausted the fixture's existing startup deadline
because the container's default Python executable runs unrelated initialization.
Selecting the installed system Python passed with the unchanged deadline.
This is not physical scanout, hardware acceleration, live-tailnet admission,
protected ingress, or WAN qualification. The Asupersync pin and decoder
preparation source are preserved. Platform GUI integration, qualified presentation
feedback, runtime codec-parameter changes and aggregate budgets remain separate.

Reproduce the explicit codec lane after installing the native SDKs:

```sh
cargo build -p fr-native --all-features --bin fr-media-worker --locked
FR_NATIVE_TEST_WORKER="$PWD/target/debug/fr-media-worker" \
  cargo test -p frd --lib actual_hevc_streams --locked -- --ignored --test-threads=1
```
