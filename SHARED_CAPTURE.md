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
check physical and logical capacity. `SharedFramePool::reserve_capacity` reserves
physical bytes and one picture slot atomically, even across cloned pools and
concurrent producers. `SharedFrameReservation::share` transfers that SAME credit
to the completed output without copying or reacquiring a slot; actual Vec capacity,
not just encoded length, determines the final charge. Drop returns unused credit.
The legacy `can_share_capacity` remains only a non-reserving snapshot.

`CaptureSource::prepare_shared_capture` joins the reservation to the original
native source before advancing a frame identity or issuing IPC. It reserves the
configured maximum access unit plus the IPC prefix retained by the parser. An
impossible pool/profile refuses; a temporarily full pool returns backpressure
without encoding a reference that would have to be silently discarded. The
returned `PreparedSharedCapture` exclusively borrows the original source while
other network owners can continue to use their own egresses. Its existing native
capture path retains authority, topology, source provenance, deadlines and the
shared recovery scheduler. The same reserved allocation ceiling reaches the
worker's `request_with_response_capacity` for BOTH the initial exchange and every
Poll reply; declared length is checked before allocation or payload reads, and
actual capacity is checked before pixels enter the buffer. Revocation between
preparation and polling refuses.

Preparation can preflight an already-joined egress through `check_recipient`,
including shared allocation metadata and that viewer's own cache metadata. This
logical-credit check does not reserve a recipient or hold its borrow across IPC;
the serialized producer must not enqueue other outputs in between. Final egress
admission still runs. A blocked viewer does not force another viewer to wait.
First-viewer admission remains a separate authenticated decoder handshake.

An unpolled preparation frees credit without native side effects. Cancelling an
in-flight capture uses the existing native-operation abort/poison path before its
parent-side output reservation is released. The child process retains its own
separate native memory bounds: this reservation is not accounting for child/GPU
surfaces. Static unchanged results release unused picture credit and carry only
fixed source-verification metadata. A returned allocation larger than the bound
is refused rather than allowing a silently broken reference chain to continue.

Broker registry size, first-viewer/last-viewer source lifetime, bounded
per-connection send scheduling and network publication across multiple viewers
still require their own OS-share-session integration. These APIs do not change
the current native publisher into an automatic shared broker, and the
eight-recipient turn bound is not a global subscription admission limit.

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

## Pre-production reservation verification

Six new physical-ledger tests cover byte/count reservation, eight concurrent
producers, cloned pools, actual-capacity overflow, release and in-place transfer.
The pinned, locked offline core/wire/media suite passes 595 tests including
doctests and strict all-target/all-feature Clippy. Eight new native preparation
tests pass alongside the eight original native-sharing tests (16 total), covering
pool/profile refusal before IPC, intact reference numbering, pending capture,
cancellation, static output, logical preflight, foreign source and revoked consent.
They use production code and supervised child IPC with opaque codec fixtures,
not real HEVC or hardware qualification.

The current native slice rebuilds all eight first-party libraries from the
checksum-verified b1b2d366 source archive plus these changes and the exact
concurrent 98186a2 worker-capacity implementation (worker blob 98416a295), with unchanged
compiler/lock-matched upstream libraries retained by workflow 35466193560 and
nightly-2026-08-31. Complete daemon test-source metadata and the affected native
integration target pass strict Clippy. Production-library Clippy still identifies
the two existing host-recovery wildcard imports described above. This is not a
cold dependency build, current combined-main full-workspace runtime pass, or
automatic multi-viewer broker qualification. Concurrent file-selection,
broker, tailnet and later viewer changes are preserved but outside this baseline.

## Independent decoder startup from shared capture

`decoder_startup::Host::new_shared` starts one already-authorized viewer from a
`SharedCaptureUpdate`. It uses the SAME host acknowledgement state machine as the
unique-output constructor, including exact connection/session binding, selected
display dimensions, bounded HEVC parameter parsing and capture-anchored expiry.
The full physical allocation charge must fit that viewer's negotiated retention
ceiling; the encoded payload length alone is insufficient.

Each startup owner retains only its own shared reference, configuration record,
consent and deadline. One viewer's delayed acknowledgement, expiry or cancellation
cannot prevent a different viewer from reaching its own decoder-configured gate.
`take_shared_recovery` transfers this viewer's original source-bound alias only
after its matching Configured record. The existing shared egress then sends its
reliable IDR, and the SAME host owner waits for the matching FirstDecoded record.
Neither acknowledgement grants input or asserts physical visibility. Calling the
unique transfer method on a shared startup (or the shared method on a unique
startup) refuses without consuming the pending output.

This is actual decoder-startup integration, not automatic viewer admission or a
second encoder. The parent OS share-session still must admit the correct source,
bound pending startup owners, rate-limit late-join IDRs, keep the shared capture
lifetime independent of any one viewer, and service each connection fairly.
It must not give every newly arriving viewer an arbitrarily old cached IDR: shared
startup preserves the original capture timestamp and cannot refresh its deadline.

Seven new `shared_startup` integration tests pass using two independently
configured TLS/UDP connections, production channel attachment, HEVC validation,
packetizers, receivers and supervised decoder child IPC. One completed native
capture and one physical allocation feed both startups. The tests cover independent
acknowledgements, per-viewer cancellation/expiry, exact single-use transfer,
unique-output compatibility, malformed/static bootstrap refusal and full logical
retention charging. Child encoder/decoder results remain explicit synthetic
fixtures, not HEVC encode/decode or hardware qualification.

After the startup refactor, all 16 shared-capture tests and 48 existing native
recovery, replacement, authority, egress and worker-supervision tests pass alongside
these seven tests (71 executed native integration tests total). Strict Clippy passes
for the new integration target and the complete baseline daemon unit-test source
metadata. The source-rebuilt daemon uses the same verified b1b2d366 + reservation /
prepared-capture baseline and exact 98186a2 worker source described above, with the
pinned compiler and matching unchanged upstream CI libraries. The two baseline
production-only host-recovery wildcard lints remain; no lint configuration or
assertion was relaxed. This is not a complete combined-current-main workspace
runtime, broker, live-tailnet, hardware or independent interoperability pass.
