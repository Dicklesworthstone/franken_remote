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

The previously published wire grammar, validation, `Requester`, `Responder` and
`ReceiverEvidence` are reused unchanged. The runtime does not alter their
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

`StreamingHost::receiver_feedback_reports` counts accepted new solicited reports.
`StreamingViewer::receiver_feedback_reports` counts reply admissions into transport.
Neither is an acknowledgement of delivery, presentation or application effects.

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

The supplied verification evidence distinguishes selected Cargo tests from
first-party runtime rebuilds against retained dependencies. No fresh full-workspace
Cargo dependency rebuild, new HEVC/hardware measurement, physical scanout proof or
live-tailnet qualification is claimed. The Asupersync pin and published decoder
preparation changes are preserved. Platform GUI integration, qualified presentation
feedback, runtime codec-parameter changes and aggregate budgets remain separate.
