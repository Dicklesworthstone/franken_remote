# Bounded late joins on a continuing shared source

`Publisher::join_queue()` returns a weak handle to the existing publisher. A
connection task can call `JoinQueue::admit` while `Publisher::serve` exclusively
owns capture. Admission consumes that connection's completed `NegotiatedMedia`
attachments and already-approved observation control. It does not create a
listener, permission, second encoder, connection, runtime, or input grant.

## One bounded lifetime

Initial pending, queued late-join, and decoded subscribers share the same eight slots. The source scope,
observation-only role, original opaque connection, independent authority/task,
and duplicate-session checks apply before insertion. The call-time timeout is
positive and at most two seconds; waiting for an IDR, configuration, and first
decode all consume that same deadline. A failed slot stays occupied until its
original non-cloneable Subscriber drops. Queue handles are weak. Queued late joins
cannot keep a source running after its existing cohort leaves. Admission through
the weak queue requires an already-decoded member, not just an initial pending
handshake. Initial `admit_pending` members retain their existing startup lifetime.

The original source's IDR coalescer admits all currently waiting viewers from
one fresh source-bound IDR, sharing the existing 500ms recovery-request allowance.
Rate pressure continues ordinary dependent/static capture for established
viewers; it does not reset their bindings, reference identities, or input leases.
A waiting viewer cannot force raw capture through all-viewer backpressure.
Physical capacity is reserved before native IPC. Every recipient independently
passes logical byte/count credit and final admission checks.

Newcomer expiry is NOT the shared worker's native deadline. Expiring a join during
a delayed native operation refuses that subscriber while preserving the output
needed by healthy viewers. Source authority, the original fixed native budget,
and existing loss-recovery caps still bound that operation. No deadline restarts
on an acknowledgement, rate retry, configuration send, or Poll response.

## Startup without a global pause

A fresh join retains aliases in its ORIGINAL bounded sender: at most four cached
pictures while startup is incomplete, further constrained by its existing byte,
count, and capture-anchored time limits. This permits a bootstrap plus a short
dependent chain while configuring, not an unbounded GOP replay. No picture is
silently skipped after the first IDR. A viewer that cannot retain the next
reference is explicitly refused before production when healthy viewers proceed.
Failure releases that viewer's bootstrap alias and sender and revokes its exact
observation authority. It does not cancel another subscriber or the encoder.

`Subscriber::service` now drives the same `decoder_startup::Host` handshake on the
original connection before sending its retained media. Configuration parsing uses
the actual source IDR and original codec configuration. Final native configuration
and packet enqueue checks include BOTH source consent and subscriber authority.
No pixel leaves until the matching DecoderConfigured reply. Configured viewers
can receive dependent pictures without a first-decode round trip per frame.
`Subscriber::is_ready` becomes true only after the matching FirstDecoded report.
That is a peer decode report, not physical visibility or authorization to control.

Configuration counts against the same bounded send turn as media. The existing
`startup_complete` query and `is_ready` agree for both admission paths. Initial
`admit_pending` retains its original stricter FirstDecoded transmission gate and
single-next-reference retention; the late-join path does not weaken that gate.

The network task must continue UDP, original session renewal/control, cancellation,
and bounded send turns. The subscriber owns only its exact decoder-reply lane;
other records remain with the original session dispatcher. The source service
includes join deadlines in idle maintenance. Explicit teardown still requires
original connection/input cleanup and `Publisher::reap` for confirmed child exit.

## Executed evidence and remaining work

Ten new integration tests use independent TLS/UDP peers, actual completed channel
attachments, supervised source IPC, host startup parsing, packetizers, receivers,
and shared storage. They cover simultaneous joins; no media before configuration;
continuing references before FirstDecoded; a four-picture stalled-join ceiling;
expiry during native work; eight-slot capacity and reuse; duplicate/foreign
admission; premature decode reports; source revocation; last-viewer departure;
and queue/network service beside the exclusively borrowed source service. One
also drives the original Viewer/Presenter through supervised decoder IPC and a
subsequent dependent frame. Codec payloads and decode completions are explicitly
synthetic fixtures, not real HEVC/GPU, physical-scanout, or live-tailnet evidence.

Final focused results: 10 late-join, 3 source join-rate, 25 existing shared-startup,
24 shared-capture, 6 fair-fanout, 9 native-recovery, 7 egress, and 3 authority tests
passed, plus all 57 fr-media unit tests. After reconciling concurrent pending
startup commit 72abf4ea, all those integration scopes passed again and all nine
unchanged upstream pending-startup tests passed (153 unique tests). Strict pedantic Clippy
passes for complete fr-media/frd production libraries and both new integration
targets; changed-file formatting and whitespace checks pass. Eight first-party
libraries were built from checksum-verified 1397d507 source using pinned
nightly-2026-08-31 and unchanged compiler/lock-matched external libraries retained
from CI source ea73e284. This is not a cold external-dependency build or a complete
current-main workspace/hardware qualification. Concurrent unrelated work is
preserved on publication, not counted as executed coverage.

First-viewer bootstrap retains both existing completed-handshake admission and
`admit_pending` for a caller-prepared, source-proven bootstrap handshake.
Wiring this owner into the OS-session listener/registry, independent source-consent
renewal, and shared failed-viewer recovery/replacement remain separate integration
work. The broad frame-pipeline qualification bead is intentionally still open.

Refs: plan sections 7, 11.2, 13.4, 17 and 19; fr-p1-frame-pipeline-am1.
