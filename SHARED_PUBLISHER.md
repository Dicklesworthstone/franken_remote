# Source-owned shared publication

`media::shared_publisher::Publisher` owns one actual native source and its existing
physical frame pool. It replaces first-viewer ownership with a bounded cohort of
independently revocable, already-decoding observation subscribers. It creates no
new encoder, transport, runtime, permission, or input grant.

## Admission and custody

Construct the owner with an actual source-bound shared capture from the SAME
physical pool. Equal frame/configuration numbers and a newly allocated pool with
identical limits do not establish ownership. The source has independent consent
and its own sibling runtime context, not the first viewer's cancellation context.

Admission consumes a completed `decoder_startup::Host`, the original shared
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
listener or a second session broker. Initial pending-viewer admission, renewal of
the independent source scope, rate-admitted late joins, shared recovery policy,
and attaching the coordinator to the OS session registry remain separate work.
No new control path, codec profile, dependency pin, or protocol limit was added.

Refs: plan sections 7, 11.2, 12.3, 17 and 19; fr-p1-frame-pipeline-am1 remains open.
