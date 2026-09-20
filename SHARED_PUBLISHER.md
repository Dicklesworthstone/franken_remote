# Source-owned shared publication

`media::shared_publisher::Publisher` owns one actual native source and its existing
physical frame pool. It replaces first-viewer ownership with a bounded cohort of
independently revocable observation subscribers, including bounded pending
decoders. It creates no new encoder, transport, runtime, permission, or input grant.

## Admission and custody

Construct the owner with an actual source-bound shared capture from the SAME
physical pool. Equal frame/configuration numbers and a newly allocated pool with
identical limits do not establish ownership. The source has independent consent
and its own sibling runtime context, not the first viewer's cancellation context.

The original `admit` consumes a completed `decoder_startup::Host`, the original shared
`QuicEgress`, and its completed `NegotiatedMedia` attachments. It checks the actual
connection, observation-only role, sender authority, source identity, current
reference, and common OS-session/display/configuration scope. Each viewer keeps
its own viewport and recovery generation. The first native decoder report is not
physical visibility or permission to control the desktop.

At most eight subscriber handles can be outstanding. A failed entry retains its
slot until the original non-cloneable handle drops; old handles cannot act on a
replacement. A handle holds only a weak publisher reference and cannot keep a
dropped native owner alive. Admission before the first subscriber has a two-second
local ceiling; the original decoder startup deadline can be shorter. Last-viewer
removal terminally revokes the source's authority; it cannot be revived by a new
subscription. `tick`, an active capture, or explicit `reap` completes native stop.
The original child stays collectable until confirmed reaping.

## Capture and per-connection service

`capture_next` uses `prepare_shared_capture` before issuing native work. The
existing pool reservation covers the entire bounded IPC allocation, including
its retained prefix; native response limits and all Poll replies keep that same
credit. Every existing recipient is preflighted independently. If none can accept
a new picture, no capture/reference is produced. If some can proceed, a lagging
recipient is explicitly refused and its authority/cache fenced BEFORE capture,
rather than silently dropping an encoded reference or stalling a healthy viewer.
Final admission checks again after native completion, including mid-capture
revocation. Shared payloads are never copied into per-viewer picture queues.

Each network task keeps its ORIGINAL HostSession and QuicRecords. A Subscriber
can service up to 64 actual packet admissions per synchronous turn while capture
is pending. There is one retained prepared record per egress, not a new FIFO.
Foreign connection objects refuse before mutation. Final transport admission
checks both source consent and the original subscriber/packet guard. Selective
repair uses only that subscriber's installed repair route; other records remain
with the original session dispatcher. The caller must continue observation
renewal, UDP service, cancellation, and bounded per-connection scheduling.

One viewer's failure/drop closes only its authority and egress. Source failure or
cancellation fences ALL affected viewers before native cleanup. The capture
future is Send and has a call-time cancellation guard, including when never
polled; during native work an inner guard fences the cohort before aborting the
pinned worker exchange. No policy mutex spans native IPC or an await. Fixed
capture/send reports describe admission, not delivery, decode, freshness or input.

## Verification and remaining integration

Twelve new tests use the actual publisher with independently attached TLS/UDP
peers, completed host handshakes, supervised native source/decoder IPC and the
production receiver/egress implementations. They cover continuing dependent
frames, independent departure, slow-viewer refusal, all-viewer backpressure,
source revocation before queued writes, foreign connections, static observations,
physical-pool identity, unfinished-decoder refusal, cancellation before and during
native work, last-viewer departure, and revocation during capture. Canned codec
parameters and synthetic child decode completions are explicit fixtures, not
real-HEVC, hardware, physical-scanout or live-tailnet qualification.

Fresh source builds passed 19 shared-startup tests (12 new), 24 shared-capture,
9 native-recovery, 3 authority, 7 egress, 4 bounded-response and 12 supervision
tests: 78 executed integration tests. Strict daemon-library and shared-startup
Clippy, changed-file formatting and diff checks pass. All eight first-party
libraries were rebuilt from checksum-verified 1bdb523 source plus this change,
with pinned nightly-2026-08-31 and matching unchanged upstream libraries retained
from CI source ba79119. This is not a cold dependency build or full-current-main
workspace qualification; concurrent session-agent/e2e/fitted-presentation changes
are preserved on publication but are outside this executed source baseline.

This owner is the bounded source/subscriber coordination layer, not an automatic
listener or a second session broker. Automatic listener admission, renewal of
the independent source scope, rate-admitted late joins, shared recovery policy,
and attaching the coordinator to the OS session registry remain separate work.
No new control path, codec profile, dependency pin, or protocol limit was added.

Refs: plan sections 7, 11.2, 12.3, 17 and 19; fr-p1-frame-pipeline-am1 remains open.

## Continuous source service

`Publisher::serve(capture_interval, report)` owns the continuing raw-capture
cadence while subscriber connection tasks run independently. The interval must
respect the configured codec frame rate and cannot exceed one second. Completion
and backpressure both schedule the next opportunity from the current time;
missed opportunities do not accumulate into catch-up captures. All-viewer pressure
pauses raw work rather than producing references that nobody can accept.

While waiting, the loop schedules consent and per-viewer retention maintenance
within ten milliseconds, or at an earlier media deadline. Timers do not certify
unchanged pixels or renew source/viewer authority. Last-viewer departure and source
revocation remain terminal; cancellation before polling or during idle sleep
fences the cohort and aborts the original worker, which remains available to reap.
The bounded report callback runs outside policy locks and reports actual capture
admission only. It receives neither pixels nor authority and cannot trigger an
encoder catch-up burst by taking time. Connections still service UDP, their
original observation renewal, and repairs on their own tasks; this source loop
is not a replacement session transport driver.

Six additional real TLS/UDP/child-process integration tests exercise this service:
continued delivery after one of two viewers leaves, all-viewer pressure, delayed
native completion without catch-up, idle source revocation, cancellation before
polling and during sleep, and invalid cadence refusal. All 25 shared-startup tests
pass with the updated daemon library. Four private cadence tests also pass in a
narrow harness containing the exact production Cadence implementation and its
unchanged inline tests. Strict daemon-library and shared-startup Clippy, formatting
and diff checks pass. Source/dependency and synthetic-codec qualification limits
remain those stated above; no current full-workspace or real-HEVC claim is added.

## Pending decoder admission

`Publisher::admit_pending` accepts an independently authorized observation viewer
on its completed media attachments BEFORE decoder startup completes. It consumes
that connection's unstarted `Host::new_shared` handshake and empty `QuicEgress`.
Actual source, current IDR, physical-pool identity, connection, full view scope and
original authority must agree. Pending and completed viewers share the same eight
slots; a failed handle retains its slot until dropped, just as an active handle.
This is not a permission grant, automatic listener, or implicit late-join IDR.

`Subscriber::service` drives the existing configuration/reply state machine within
its ordinary bounded turn. Configuration counts against the same send budget as
media. The shared IDR reaches this viewer's original sender only after its exact
Configured reply; dependent records are held until its matching FirstDecoded.
Holding a prepared dependent packet retains its original bytes and deadline.
`startup_complete` reports only that peer-reported decode gate, never visibility
or input readiness. The original connection owner still drives UDP, observation
renewal, and unrelated records during native decoder work.

All unconfigured viewers cause raw-capture backpressure without advancing the
source. A configured pending viewer can retain one next reference in its existing
bounded cache while awaiting FirstDecoded. If it cannot drain before another
healthy viewer advances again, it is explicitly fenced before capture, not given
an unbounded catch-up queue. Unconfigured laggards likewise cannot hold a healthy
viewer back. Each original startup deadline is included in continuous source
maintenance even without packets. Expiry, wrong acknowledgements, cancellation
and departure affect only that viewer unless it was the last remaining member.

Nine new tests exercise real TLS/UDP and supervised synthetic decoders: separately
completed gates and resumed dependent frames; unconfigured and first-decode
laggards; exact cohort exhaustion before configuration sends; idle expiry and late
replies; wrong first-frame reports; foreign connections/sources; and last-viewer cleanup.
The final startup runs pass 25 existing and nine pending-startup tests; the
rebuilt capture, recovery, authority, egress and worker group adds 59 passes.
Strict daemon-library and complete startup-target Clippy and changed-file
formatting pass. An earlier full startup run hit a native deadline in the existing completed-viewer
test; the unchanged subsequent full run passed. That earlier failure is retained,
not represented as resolved hardware or timing qualification.

All eight first-party libraries were rebuilt from the checksum-verified 4b1c9b9
source snapshot and this change, with pinned nightly-2026-08-31 and unchanged
compiler-matched retained external CI libraries. This is not a cold dependency
build, complete workspace test, live-tailnet, real HEVC or hardware qualification.
Concurrent source late-join admission work is preserved at publication; automatic
pending listener integration, independent source-scope renewal and shared recovery
remain open. Existing `admit` remains the completed-handshake entry point.
