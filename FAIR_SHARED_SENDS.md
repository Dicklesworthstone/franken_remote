# Fair shared-media sending

`media_quic::fanout::SendSet` owns at most eight existing negotiated senders for
one actual capture source. It joins completed native decoder-startup owners to
source-bound shared output and the original QUIC connections. It creates no
listener, native worker, transport runtime, new packet FIFO or input grant.

## Admission and ownership

`ReadySender::new` consumes the existing host decoder handshake and its sender.
The handshake must have received its matching FirstDecoded record; the original
connection, subscription authority, view and native-source provenance must all
agree. Calling this constructor too early cannot turn a configured decoder into
a completed one. A constructor failure retires its moved sender, not another
connection. The parent retains responsibility for native key/button release.

`SendSet::insert(&mut Option<ReadySender>)` moves the sender only after successful
preflight. Full sets, duplicate remote sessions, foreign sources and terminal
senders refuse without consuming the pending owner or allocating a large error.
Slot reuse changes a monotonic serial. A handle also names its actual set, so
identical slot numbers from another set or a removed member cannot redirect work.
The original source identity remains bound even when every member has left.

`detach` transfers the same sender back to a session/recovery owner, preserving
its cache, pending packet, recovery allowance and repair history. `retire`, set
closure and owner Drop fence that viewer's input readiness before releasing its
retained egress. They never cancel the capture worker or another viewer. Terminal
egress closure now applies this readiness fence on its error paths too.

## Bounded fair native turns

Publish one actual `SharedCaptureUpdate` to the set. Each recipient uses its own
existing sender/reference/authority checks and logical retention limits. Foreign
source output refuses before changing any member; one failed recipient does not
skip subsequent members. The shared pool continues to account for physical bytes
once, while each viewer remains charged for the full history it can retain.

`transmit` takes the complete handle-to-connection mapping. It validates ALL
handles, uniqueness and opaque connection identities before touching any clock,
sender or native queue. Each retained member must appear exactly once, including
failed entries awaiting removal. An invalid mapping does not advance the cursor.

A turn is bounded by 1..64 visits, not just successful sends. Each visit admits
at most one record through the existing `QuicEgress::transmit`; an idle chosen lane
may try the other lane once. Originals and repairs alternate per member, while an
already prepared packet keeps precedence and its unchanged original deadline.
Backpressured or idle members are skipped for the rest of the turn. The round-robin
cursor survives across calls, including calls admitting only one record. This is
record-opportunity fairness, not a new congestion controller or byte-rate promise.

Every native send still uses the original egress guard and transport admission.
Returned counters mean transport admissions, not delivery, decode or visibility.
Reports have fixed-size storage, identify each member, and retain the first typed
failure until the parent removes it. Closed entries cannot silently disappear.

Service `tick` and the minimum `next_deadline` even without capture or socket
traffic: shared bytes and pending packets must not be pinned by an idle viewer.
These are media deadlines; the original session's authority/renewal/watchdog must
continue independently. Set cancellation fences its members but does not close
unrelated sockets or the shared source. The original parent owns connection and
native-input cleanup.

## Scope and evidence

Six integration scenarios use two independently established TLS/UDP connections,
production attachments, host/viewer decoder handshakes, supervised child IPC,
packetizers, receivers, shared storage and input authority. They cover fair turns
under actual native send backpressure, independent departure and expiry, mapping
preflight, stale/foreign handles, intact capacity refusal, foreign source output,
closed native connections, explicit original-sender transfer and cancellation.
The 27 existing shared-capture, egress, native-recovery and authority tests also
pass. The complete baseline daemon test source and new integration target pass
strict Clippy; changed files pass formatting and whitespace checks.

The eight first-party libraries were rebuilt from the checksum-verified b1b2d366
source snapshot plus this slice, using nightly-2026-08-31 and unchanged matching
upstream libraries from retained CI run35461102442. The test harness uses a 16 MiB
thread stack for the unoptimized composed native fixtures. Current reserved-capture
APIs are preserved by exact preimage hashes when publishing; subsequent broker,
CLI, tailnet, file and native-reservation changes are outside this executed runtime
baseline. Two pre-existing production-only host-recovery wildcard-import lints
remain in that baseline. No lint configuration or assertion was weakened.

Child media and decode completions are explicit fixtures, not real HEVC/GPU or
live-tailnet qualification. This is a native multi-connection send owner, not a
complete automatic multi-viewer broker. The OS share-session must still own source
lifetime, stop capture after its last subscriber, bound pending joins and IDRs,
reserve physical/logical capture capacity, and drive each original session's
control/renewal between send turns. Detach to the existing recovery coordinator
before replacing an individual member's media bindings; do not allocate a fresh
sender to erase recovery history.

Refs: plan 7, 11.2, 12.3 and 19; fr-p1-frame-pipeline-am1.
