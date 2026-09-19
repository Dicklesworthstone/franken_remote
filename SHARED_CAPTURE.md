# Shared encoded capture delivery

`SharedFramePool` retains one physical encoded allocation while independent
`SendCache` instances charge each viewer for the full storage it can pin. Actual
Vec capacity, shared allocation metadata and reference counters are counted.
Cloning a pool does not create credit; cloning a frame does not copy its payload.
The last frame owner releases the bytes before returning their pool reservation.
Pool bytes and frame count remain bounded even if a publisher retains an alias
after all senders close. The legacy single-viewer Vec path remains available.

`CaptureUpdate::share` carries the original native source identity into a
`SharedCaptureUpdate`. Only a completed, validated worker result can create this
proof. Encoded results use the shared pool; unchanged-source results carry fixed
metadata without allocating another encoded picture. Frame identity, dependency,
codec generation and capture time are immutable. Each recipient supplies its own
negotiated packet stride, channel bindings, recovery generation and deadlines.

## Native fanout and failure isolation

The existing `Subscription`, `Egress` and `QuicEgress` accept source-bound shared
updates through `enqueue_shared_capture`. `SharedCaptureUpdate::distribute` serves
up to eight already-admitted egresses in a synchronous bounded turn and returns
one fixed-size, ordered admission result per recipient. Failure of one recipient
does not skip later recipients. This creates no packet queue, native operation,
async runtime or transport acknowledgement.

A source/configuration mismatch refuses before changing a healthy subscription.
Once provenance passes, expired authority, a missed reference, or exhausted
viewer capacity invalidates that viewer's readiness/input tickets and closes its
cache rather than pinning shared history or silently omitting reference pictures.
Other viewers and the capture worker are not cancelled. Native input cleanup and
connection retirement remain the original parent owner's responsibility. The
ordinary final per-packet authority/pacing checks still apply to prepared packets.

The same actual source-generated IDR may use a fresh viewer's reliable recovery
lane and a healthy viewer's independent datagram lane. Neither that healthy
viewer's recovery generation nor its input authority is reset. Source recovery
requests still pass the existing sender policy and native source scheduler; this
API does not bypass their rate histories or implicitly grant/reacquire control.

## Admission and integration boundaries

The OS share-session must explicitly admit each viewer for the correct source and
display before its first IDR, just as for `enqueue_capture`. Possession of a shared
update is not an identity or permission grant. Keep the same physical pool across
viewers and recovery generations; sharing multiple encoders requires an aggregate
pool/owner rather than independently multiplying its allowance.

Before producing a picture, service each viewer's idle/retention deadlines and
check physical and logical capacity. `can_share_capacity` is a non-reserving
snapshot for a serialized producer, not a permit for simultaneous encoders.
`share` receives an already allocated output: the producer must bound allocation
before native capture, not after it. Broker registry size, first-viewer/last-viewer
source lifetime, bounded per-connection send scheduling and network publication
across multiple viewers still require their own OS-share-session integration.
This does not change the current native publisher into an automatic shared broker,
and the eight-recipient turn bound is not a global subscription admission limit.

## Executed verification

Ten production packetizer/receiver tests in `shared_delivery` cover physical
pointer identity, ledger lifetime/capacity/count bounds, logical viewer charges,
independent strides, selective repair, capture-anchored deadlines, slow-viewer
isolation and recovery beside a healthy reference chain. The initial pinned,
locked offline core/wire/media run passed 586 tests including doctests and strict
Clippy. The native follow-up uses the full source snapshot at f8d9c5f plus that
four-file change, preserving concurrent source/authority/recovery work. The
refreshed core/wire/media suite passes all 589 tests including doctests and strict
all-target/all-feature Clippy. Another 31 existing native recovery, authority,
egress and worker-supervision runtime tests pass alongside the eight new tests.

Eight `shared_capture` tests use real supervised child IPC, production native
source/authority/egress owners and production receivers. They verify one physical
buffer across two viewers, allocation-free static observations, independent
revocation including a prepared packet, missed-reference input fencing,
foreign-source refusal, per-viewer pressure, one native IDR for two generations,
and preflight fanout-turn bounds. Decoder completions are simulated and child
payloads deliberately opaque: neither suite qualifies HEVC, GPU/display hardware,
live tailnets, independent interoperability or an automatic multi-viewer broker.

Native libraries are rebuilt from source with nightly-2026-08-31 against unchanged
compiler/lock-matched upstream libraries from checksum-verified CI run35461102442.
The refreshed source archive from run35464981335 was independently checksum-checked;
its Cargo.lock matches. Native test execution is not a cold dependency/workspace
build. Full daemon test-source metadata and the new integration target pass strict
Clippy. The production library's strict Clippy run reports two existing wildcard
imports in the running-host recovery modules; no lint configuration was weakened.
A full daemon runtime binary build exceeded the execution limit without producing
results, so no full daemon/workspace runtime pass is claimed.

Refs: comprehensive plan 7, 11.2, 12.3 and 19; fr-p1-frame-pipeline-am1.
