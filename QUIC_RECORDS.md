# Native Asupersync QUIC record transport

`fr-transport::quic::QuicRecords` connects FRD0 records to the real
`NativeQuicUdpConnection` of the workspace-pinned Asupersync (0.4.10 when this
slice landed and in the 0.4.10 statements below; the workspace pins `=0.5.0`
since). QUIC is primary. No WSS implementation, alternate QUIC stack, executor, public listener,
account system, or synthetic handshake is introduced.

## Implemented boundary

The caller transfers exclusive ownership of an already established native
UDP/TLS connection with ALPN `fr-remote/0`. The constructor refuses early-data
states, another ALPN, previously used outgoing streams, invalid directions,
duplicate bindings, and overly large native receive windows. Routes come from
the admitted local session owner, never from a peer's claimed role.

The adapter supports up to sixteen bound unidirectional reliable streams and four
bound datagram routes. Each route has one exact message kind. This implements
the media progress/recovery/repair split, not the still-incomplete negotiation,
zero-binding bootstrap, all-kinds control stream, or multi-client listener.
Message-specific codecs and live authority checks remain mandatory.

Reliable stream bytes pass through `fr-wire::stream::RecordStream`, which
retains one complete bounded record per route across partial/coalesced QUIC
reads. Extra read bytes stay in one bounded transport buffer. Header validity,
record size, binding, fixed first-byte deadlines, truncated FIN, and terminal
failure are enforced. A blocked consumer retains the record rather than
silently dropping a reliable action. Neither trickle input nor repeated polling
renews its lifetime. Datagram payloads remain individual records; stale bindings
are disposable without allocating replies.

`send` admits a whole prepared record or returns backpressure without admission.
The caller keeps the SAME packet returned by the media sender on backpressure;
it must not ask the media sender for the next packet and skip this one. Reliable
records are emitted in small native stream writes by `drive`, so a large FRD0
record does not become an oversized or congestion-window-sized QUIC write.
Admission consults Asupersync's actual flow/congestion state; it does not invent
another congestion controller. The conservative application datagram ceiling
is 1,150 bytes, below the pinned 1,200-byte protected-packet limit.

## Ownership and cancellation

Authority callbacks and the real runtime clock are checked on admission, again
before copying into native work, before each reliable prefix, and after I/O.
An expired partially emitted record closes the connection; its remaining bytes
are never reinterpreted as another record. A fixed conservative batch deadline
also bounds fully staged reliable buffers awaiting release. The session's
independent watchdog must cancel the Cx on revoke/expiry while I/O is suspended.
This adapter does not create observation authority or input leases.

Receive callback boundaries (2026-11-24, `fr-inbound-dispatch-authority-47l6`).
Every retained datagram candidate and reliable-record dispatch rechecks the
original connection lifetime, operation guard, per-call authorization,
cancellation and monotonic clock. Readiness is a local callback: returning true
cannot override a stop or extend a receipt deadline. A blocked datagram handler
cannot permit another retained route or native arrival to dispatch under the
authority sampled before that callback. Reliable framing and validation finish
before the final dispatch check, and the receive turn checks again before
advancing connection flow credit.

Those checks can retire an optional lane or expire a held audio packet. Dispatch
revalidates the exact stream route and readable state. A successor promoted into
a datagram slot needs its own readiness callback and keeps its original receipt;
neither expiry nor a skipped candidate consumes another route's live record.
Actual UDP/TLS fixtures in `tests/receive_dispatch.rs` exercise all four stop
sources, blocked-route scanning, receipt expiry and unchanged live receipts.
The clipboard attachment regression retires the pair inside readiness and
continues control/input traffic in both directions. Native qualification remains
separate from this transport-framing evidence.

During async I/O the connection is moved out of the owner. Dropping the drive
future drops that connection rather than allowing unsafe continuation after
partial effects. Completed or cancelled work cannot be replayed on a new
connection. The parent still performs its normal input revocation and cleanup.
A synchronous receive handler must not block on capture, decoding, presentation,
or input injection; these already have separately owned native workers.

Optional-lane retirement during native I/O (2026-10-26). A clipboard Off or
other established optional-lane retirement discovered during an I/O poll can
preserve the parent connection only when native application payload was proven
empty at that operation's entry. The proof includes every known outbound
stream's pending and retained retransmission state, the native DATAGRAM queue,
complete stream accounting, and uncertainty retained from earlier incomplete
native operations. This is connection-wide evidence: unrelated input, video or
file payload also makes an entry ineligible. An application record still queued
outside the native owner is not protected output and remains removable by the
ordinary retirement path.

With that immutable entry proof, the same exclusively borrowed native future
finishes under its original cancellation, immutable lifetime, authorization,
clock, attachment and delivery-deadline checks. Peer packets can generate
transport replies and incoming data, but cannot turn queued FRD0 records into
outgoing application bytes. Before further application staging or dispatch,
the owner retires the exact optional pair and discards its receive state. This
keeps idle Off actions from unnecessarily ending input, the display or another
file direction. It neither extends the receive/drive budget nor drops a pending
native future to pretend it completed.

A staged stream prefix, retained retransmission, queued DATAGRAM or uncertain
protected output keeps the existing terminal `OptionalRetirementDuringIo`
fence. A post-return empty queue is not sufficient proof. Authority loss,
cancellation, native failure and a dropped outer future remain terminal, and
terminal-report drains retain their original conservative guard.

The actual UDP/TLS attachment regressions in `clipboard/retire_io.rs` cover a
pending empty receive with and without late peer clipboard bytes, continued
input/control on the original connection, retained stream payload, queued native
DATAGRAMs, lifetime denial and cancellation. These new runtime tests have not
been executed in this resource-constrained session; source review of the pinned
native implementation and formatting are separate evidence from a live pass.

### File receive deadlines

An established `Files` or `FilesDownload` attachment now retires independently
when its original first-byte receive deadline expires. This covers a stalled
header or body as well as a complete record waiting for its consumer. The
connection owner resets both directions of that exact file pair, frees its
framing and queued-send storage, and keeps the consumed ticket, binding and
stream identities as tombstones. Late bytes cannot reopen the transfer or be
interpreted as a new record. The other file direction, input, control and media
remain on their original connection.

`FilesChannel::check` reports the retained framing-expiry cause to its original
file owner. The receiver fences its disk worker and collects actual cleanup;
the sender preserves publication receipts or uncertainty without replay. CLI
completion distinguishes `receive_expired` and `interrupted_expired` from a
generic cancelled transfer. A previously published file remains published if a
later record expires.

The native reset code `FILES_RECEIVE_EXPIRED` (`0x4652_4645`) carries this
terminal reason to the peer. It is recognized only on the exact original
`ACTIVE` Files pair; an unfinished attachment cannot use it to avoid the
connection fence. It grants no authority, retry or stream-reuse permission.

This exception requires a completed file attachment. Malformed records,
incomplete ticket exchanges, critical-stream expiry and authority loss keep
their connection-fencing behavior. Completed Clipboard attachments also retire
on their original partial/complete receive deadline; see
[NATIVE_CLIPBOARD_TRANSPORT.md](NATIVE_CLIPBOARD_TRANSPORT.md). Outbound delivery deadlines and native-I/O
cancellation rules are unchanged; this is not a general promise that any slow
transfer or loaded desktop will survive.

Regression coverage lives in the real UDP/TLS attachment cases under
`crates/fr-transport/tests/media_attachment/files/expiry.rs` and the actual
disk-worker cases in `crates/fr-files/tests/native_quic/receive_cleanup.rs`.
These fixtures are separate from loaded two-machine and hardware qualification.

## Bounded resources

Default bounds are 64 KiB per native stream receive window, 512 KiB native
connection receive credit, 128 KiB/128 records of admitted reliable sender
storage, sixteen pending outgoing datagrams (at most 1,150 bytes each, and only
while congestion credit covers them), and two-second partial-record lifetime.
Senders refill that queue only between `drive` calls, and each drive waits for
ingress after flushing it, so the bound is one turn's worth of records: a
multi-fragment picture leaves in about `fragments / 16` turns rather than one
receive wait per four fragments. With four, a 23-fragment periodic IDR spent
about 40 ms in transit on a loaded host and missed the receiver's 50 ms display
budget, which then started at the first fragment (since 2026-11-19 it starts
when the picture is whole; PRESENTATION_FRESHNESS.md). Likewise one `receive` turn drains
at most sixteen records, one per lane per round; idle or blocked lanes do not
spend that budget, so a viewer with several quiet streams takes a burst of
fragments in one turn instead of two per call.
The explicit receive-framer capacity and retained read remainders are reported
separately. Native packet/reassembly metadata and Asupersync's bounded incoming
datagram queue are additional resources; the containing broker must account for
them as well as TLS, media caches and decoder surfaces. These are not process-RSS
measurements or a claim that all native allocator overhead fits the payload cap.

Recovery admission headroom (2026-10-26, `fr-cf5w`). While an outbound reliable
recovery route is live, non-recovery Bulk admission leaves one record slot and
the largest live recovery route's negotiated maximum inside the existing Bulk
pool (normally 8 KiB within 128 KiB). All retained Bulk bytes and records count,
including recovery records and unacknowledged native epochs. Inbound-only,
retired, finished, reset and stopped recovery senders reserve nothing. Retiring
the last recovery route releases its reservation without clearing unrelated
data. The reservation covers one largest record, rather than the sum of every
recovery lane; the ordinary full quotas still apply to recovery and data alike.

A record that cannot coexist with the reservation may borrow it only when both
retained Bulk bytes and records are zero. This permits progress with a tiny or
custom profile while keeping the original quotas, caller send-by and admitted
delivery deadline. Further non-recovery data cannot also borrow the reserve.
The rule isolates admission capacity; it cannot preempt admitted data or guarantee
congestion throughput. Fifteen exact-source admission unit tests pass on the
pinned nightly (six existing critical tests and nine recovery tests). Five new
real UDP/TLS attachment regressions cover byte/slot saturation, route direction,
retirement and the minimum valid attachment profile; they have not been built
or executed in this resource-constrained session. Their synthetic record
payloads test transport ownership, not typed application delivery or WAN loss.

Reliable bytes stay charged after native packet assembly. Merely draining
`pending_stream_data_bytes` is not evidence that retransmission copies died.
Since 2026-11-12 (fr-00nc) each record is released by Asupersync's
`QuicStream::acknowledged_send_offset` (a9705afe8): the offset below which the
peer has acknowledged every byte, which is the lowest offset still queued for
packet assembly or retained for retransmission. When a record's last prefix
enters the native stream, the adapter keeps the stream's `send_offset` as that
record's end. The record keeps its byte and count charge until the acknowledged
offset reaches that end. ACKs above the first unacknowledged byte release
nothing, and a locally reset lane reports no offset and releases nothing.

The terminal paths must prove a whole lane empty, FIN or reset state included.
They still use the older absence proof: an EMPTY outbound `QuicStream` value
saved before the first write, with its public scalar counters refreshed and the
type's structural equality as the test. It never clones live payload maps or
parses Debug output, and any unmatched private state refuses. It must be
requalified on an Asupersync upgrade.

Staging priority. Asupersync assembles each packet from control frames, then
STREAM frames, then queued DATAGRAMs. A critical reliable prefix (at most 900
bytes) is therefore staged whenever the congestion window has room for one
packet beyond the bytes in flight, even while media datagrams are queued. Those
datagrams are replaceable. The first version of this rule asked for two
packets. After loss the window sits at its 2 x 1200 byte minimum, so that
starved critical records whenever anything at all was in flight. Diagnostic
prints during busy lossy rows showed the room at 1,594-2,399 bytes with nothing
queued and the renewal challenge unstaged, and the one-packet rule removed
every such block. Until 2026-09-28 a critical prefix instead waited until the window
also fitted every queued datagram. Under namespace `tc netem` loss, media kept
that queue at the window limit, so host renewal challenges missed their send-by
deadline and closed view-only sessions. Bulk prefixes still wait for the media
queue, so a file or recovery transfer never gets ahead of media. Records on one
stream no longer wait for earlier records' acknowledgement. Until 2026-11-12 the
whole-lane absence proof was the only release test, so each stream sent in
stop-and-wait batches ("epochs"). A record admitted after a batch's first prefix
could join it only within a fixed 10 ms (bulk) or up to four records
(critical). Otherwise it waited for that batch's ACK before its first byte was
staged, up to one more round trip per record. On a 100 ms path that allowed
about one 8 KiB clipboard or file record per round trip
(fr-clipboard-wan-item-ceiling-6mlb). Now a record is staged as soon as
congestion, flow credit and per-stream FIFO allow. All byte/count limits,
critical connection credit, per-stream FIFO order and delivery deadlines are
unchanged. A staged record's original delivery deadline still covers its
acknowledgement.

### Acknowledgement evidence after a local scheduling pause

The host or viewer task may resume after a retained record's delivery deadline
even though the peer has already acknowledged it. Judging the send before
reading that queued acknowledgement mistakes the local scheduling pause for
an unresponsive peer. `QuicRecords::reconcile_acknowledgements` gives this case
one bounded observation before the ordinary retained-send deadline check.
`drive` includes it within its existing 250 ms turn bound. Host, viewer,
controlled-viewer and recovery service loops also invoke it before their first
synchronous maintenance, send or receive operation.

When no retained-send deadline is due, reconciliation does no I/O, invokes no
authorization callback and mutates nothing. It is not an authority check and
cannot replace `tick`, `send` or `receive`. Those synchronous operations retain
their original semantics: they enforce deadlines without reading the socket.
Normal maintenance therefore keeps its existing ordering and scheduling cost.

An overdue send is eligible only when its complete record has entered the native
stream and no expired unsent or partially staged record remains. Unsettled native
I/O also refuses this path. For eligible work, the pinned Asupersync
`acknowledge_io_once` receives at most one batch of 32 UDP packets with a positive
1 ms receive wait and a 10 ms absolute operation bound. It sends no application
payload, retransmission or PTO probe. It may acknowledge incoming packets, but
it dispatches no application record. No loop waits for another acknowledgement.

Before Asupersync 0f955f9d8 (pinned 2026-11-19), that ACK-only step capped its
receive wait at the connection's PTO deadline, like the transmitting drive.
Only the transmitting drive services that timer, so once a sender had slept
past its PTO every ACK-only step computed a zero wait and returned without
reading the socket. Reconciliation then always expired the record, and the
terminal settle loop (`lifetime/terminal.rs`) spun without reading until its
own deadline. The ACK-only wait is now bounded only by the caller's timeout
and the idle deadline. The regressions below failed in every CI run from
565a0201 (2026-11-14) until that pin. Their negative cases had passed only
because nothing was ever read.

Cancellation, immutable connection/operation guards, the caller's permission,
clock monotonicity and attachment lifetime remain checked before and during the
read. Optional receive expiry and explicit local or peer retirement still win.
Only local retained-send judgement waits for this read. After successful I/O,
actual authenticated `acknowledged_send_offset` evidence releases fully
acknowledged records; every remaining send is judged against its original
deadline. A silent peer still closes the connection or retires its completed
optional pair under the existing rules. A cancelled, failed or dropped native
operation remains terminal.

This changes the order of observing delivery evidence, not a record's deadline.
It cannot establish the exact time an ACK arrived at the socket: an ACK observed
by this bounded read may discharge already-sent custody after the local deadline.
It cannot make expired outgoing bytes eligible for transmission, restore an
input lease, extend a ticket, make an old view fresh, or replay a transfer.
Usefulness and authority remain checked where the content is used.
A new scheduling pause between reconciliation and a later synchronous check
can still expire that check's retained work. This is not a promise that arbitrary
host stalls, expired authority or an interrupted native operation can resume.

Regression cases in `crates/fr-transport/tests/native_quic.rs` and its
`native_quic/reconciliation.rs` module use real UDP/TLS and cover a queued ACK
after a sender pause, silence, unsent expiry, no application flush, unchanged
synchronous expiry, cancellation and original authority gates. The clipboard
attachment case verifies that the same evidence preserves an established
optional pair. These transport fixtures and the host/viewer loop regressions
are separate from loaded two-machine or hardware qualification.

Evidence for the per-record release (2026-11-12). The source is namespace
`tc netem` rows of `real_impairment::controlled_session_under_namespace_delay_and_loss`
(in-process `frd` host, shipped `fr` client). Arm A was 77100ab0 on the same
Asupersync pin; arm B was A plus this change. Both carried print-only close
diagnostics. There was no WAN or tailnet run.

- Latency: at 120 ms RTT with 1% loss on csd, the presented-age p50 was
  190-300 ms in A (median 230 ms, 24 runs) and 140-150 ms in B (24 runs). At
  40 ms RTT it fell from 60-100 ms to 60-90 ms (trj, 8 rounds; csd, 7 rounds).
- Files: a 1 MiB file finished 15-18% sooner at 120 ms (trj).
- Endings were not reduced:
  - At 120 ms with 1% loss, A ended 1 of 24 runs and B 3 of 24 (csd), and
    1 of 10 versus 1 of 12 on trj.
  - At 120 ms with 5% loss, almost every run ended in both arms (csd: A 21 of
    23, B 22 of 24), mostly `video_startup_expired`.
  - Both arms show the same startup failure. A critical stream's first
    record (host or client) is never acknowledged before its delivery
    deadline, after only one to three lost packets on the connection. Of all
    logged rows (A 299, B 359), 5 of A's endings and 8 of B's had this
    signature.

Delivery allowance (2026-09-30). A reliable record's send-by is its usefulness
deadline, enforced at admission: a record already past it is refused. Until
2026-09-30 the send-by was also the deadline for staging and acknowledgement,
and missing it closed the connection. A 250 ms record on a 120 ms RTT path
waits one round trip for the previous epoch and another for its own flight and
ACK. Any loss recovery comes on top of that. In namespace runs at 60 ms one-way
delay, control and view-only sessions ended with `transport_deadline_expired`
while both peers were healthy. An admitted reliable record's staging and retained
acknowledgement deadline is max(send-by, admission + `delivery_allowance_us`),
with the bounded observation-order exception above for already-staged bytes:

- The allowance is seven RFC 9002 probe timeouts, 7 x (smoothed RTT +
  max(4 x RTT variation, 1 ms) + 25 ms maximum ACK delay): the span of three
  consecutive backed-off probes (1 + 2 + 4). It was three until 2026-10-13
  (below).
- It is clamped to [1 s, 2 s]. The floor was 250 ms until 2026-11-12 (below).
- Before the first RTT sample, the RFC 9002 initial RTT (333 ms) gives the 2 s
  ceiling. The native handshake leaves no sample, so the first records get it.

A peer that stops acknowledging still closes the connection, within 2 s. A late
acknowledgement means a slow path, not a stale record. Content lateness is
enforced where the content is used: input tickets and lease expiry at the host,
the presented report's source age, and recovery deadlines.
`native_quic.rs` tests:

- the formula and its bounds;
- a 2 ms send-by record whose peer is silent for 60 ms keeps the connection
  and is delivered;
- a peer that never acknowledges closes the connection in bounded time: near
  the floor on a measured loopback path, within the ceiling on an unmeasured
  one.

`native_bulk/epochs.rs` states that a later record never extends an earlier
record's deadline. Planted negatives fail these tests: deliver-by = send-by, and
ignoring the measured RTT.

Seven probe timeouts (2026-10-13, fr-xfvs). Three probe timeouts were meant to
cover the previous epoch, the record's own flight and one loss recovery. On the
pinned stack one tail loss alone takes about that long: its probe timeout sends
only a PING, so the lost data waits for the PING's acknowledgement before it
is declared lost and resent. Instrumented namespace runs at 40 ms RTT and 1%
loss showed a single 67-byte host progress record unacknowledged for 248 ms
after one probe timeout, which closed a healthy connection at the 250 ms floor.
At 40 ms RTT the allowance was then about 480 ms (1 s since the floor below);
at 120 ms about 1.9 s; 200 ms RTT reaches the 2 s ceiling. A silent peer still closes the connection within
2 s, and content lateness is still enforced where the content is used. In one
controlled matrix run each, the lossy 40 and 120 ms rows held 29 and 32 steps
in total against 24 before, and the 40 ms, 1%, 5 Mbit/s row held. Most
remaining lossy-row ends are not this deadline (fr-xfvs notes).

One-second floor (2026-11-12). The allowance is computed once, at admission,
from the path estimate at that moment. The impairment harness starts each row
on a clean link and then applies netem. A record admitted just before that RTT
step therefore got the floor allowance, 250 ms, from a clean-link RTT of a few
milliseconds. One loss after the step outlasted it, and the connection closed
with Expired while both peers were healthy. Real paths can step the same way:
a tailnet direct path that falls back to a relay, a Wi-Fi roam, or queueing
behind a burst.

- Trace: STREAM-lifecycle prints in Asupersync were used for this diagnosis
  and never committed. They showed a client control record lost in one
  packet, declared lost and retransmitted about 130 ms later. The record's
  deadline, about 250 ms after admission, passed before the retransmission's
  ACK could return. Records admitted after the step had 1.5-1.9 s.
- Signature: the same one appeared on the host, an expired critical stream
  holding its first four records with one lost packet. It was 13 of the
  endings in about 650 rows, in both arms of the fr-00nc A/B.
- A/B: 12 alternating rounds on csd, 250 ms against 1 s floors, rows at
  120 ms RTT with 1% loss at 5 Mbit/s, and with 5% loss unlimited and at
  5 Mbit/s:
  - This signature ended 4 sessions at 250 ms and none at 1 s.
  - The rows held 179 and 202 of 288 steps.
  - Endings were 24 and 22 of 36. Most remaining ends at 5% loss are renewal
    lapses (Unauthorized at the host gate) and `video_startup_expired`, not
    this deadline.

A silent peer still closes the connection within 2 s, now no sooner than 1 s
on a fast path. Input tickets, leases and content freshness keep their own
deadlines.

This distinction matters on 0.4.10: attaching bounded windows in both directions
can generate continuing ACK/window-update traffic, so total bytes-in-flight
need not become zero when all reliable payload ownership has ended. Payload
credit recovery must not depend on network silence or guessed ACK counts.
The adapter does not change that upstream behavior or claim an idle-CPU gate.

## Reproduction and evidence

```sh
cargo test -p fr-wire --test stream --locked
cargo test -p fr-transport --test native_quic --locked
cargo clippy -p fr-transport --all-targets --locked -- -D warnings
```

The live tests require OpenSSL and Linux's native networking/reactor support.
They generate an ephemeral CA and localhost certificate in a private temporary
directory. No test credential, real screen content, tailnet secret, certificate
exception, or internet-facing listener is embedded in the library.

The local development run rebuilt the actual first-party crates with the pinned
compiler and linked the retained, exact Asupersync 0.4.10 TLS libraries. Eight
live tests passed, including 32-KiB stream reassembly, concatenated records,
bounded send retry, final authorization refusal, receive expiry, cancellation
and dropped I/O, actual hostname/ALPN refusal, a 1,150-byte datagram, and twelve
production media sender/receiver pictures. Both serial and parallel execution
passed. These runs are actual local UDP/TLS, not an independent QUIC peer,
Tailscale admission, a fresh full Cargo workspace rebuild, GPU qualification,
physical latency measurement, or a remotely usable desktop release.

The first large-record test exposed native packet assembly attempting to exceed
its congestion window; small staged prefixes now avoid that over-admission.
The initial zero-flight credit test exposed continuing ACK/window traffic; the
exact stream-ownership proof now releases credit correctly without treating
queued-byte counts as retained-byte counts. Both original assertions are retained.
The pure record framer adds seven independent boundary/lifetime tests.

This advances `fr-p1-fr-transport-pug` and `fr-fr-wire-framing-i0u`. Their wider
acceptance criteria remain open. External interoperability, real tailnet paths,
concurrent admission, session negotiation and the upstream QUIC fixes continue
to require their own qualification; this slice does not take over that work.

Ticketed native configuration/reply pairs and their shared route ceiling are
implemented in [NATIVE_MEDIA_ATTACHMENT.md](NATIVE_MEDIA_ATTACHMENT.md).

## Replaceable absolute-pointer receive custody

A blocked input consumer no longer makes a burst of ordinary absolute-pointer
datagrams park all subsequent datagram routes. The receive owner coalesces only
complete, canonically decoded, extension-free PointerState records on the same
route with exactly matching session, lease, ticket and all four view generations.
It retains the greatest pointer sequence. A newer state keeps its own original
receipt and expiry; a duplicate or reordered state cannot renew the held record.

This adds no queue or allocation. Different input scopes, malformed payloads and
extension-bearing records retain their original held/parked handling. Video
reference fragments remain lossless within the existing bounded custody. The
original input owner still checks authority, ticket expiry, view and click/scroll
barriers immediately before native submission; coalescing creates no receipt
claiming that discarded motion reached the OS.

The production custody tests cover scope changes, zero initial generations and
sequences, reordered/duplicate motion, expiry, retirement and fixed byte bounds.
The localhost UDP/TLS regressions exercise both readiness and handler
backpressure while unrelated audio/video records progress. These are source
regressions; native input and physical-path qualification remain separate.

## Video admission at the congestion-window floor

The native DATAGRAM owner charges one maximum protected packet for each queued
datagram. It reserves an additional packet only while this original connection
has reliable application records or native STREAM frames waiting to send. This
keeps room for both critical and bulk reliable traffic without permanently
withholding an empty packet from a video-only flight. At the two-packet loss
floor, both video slots can therefore be used when no reliable work is pending.
Already-transmitted reliable bytes remain charged by the native congestion
controller; waiting for their acknowledgement does not reserve a second empty
slot. Critical records admitted after video still stage first.

The 128-datagram queue ceiling, packet-size bound, authority checks, original
send expiry and native congestion/pacing checks are unchanged. Helper regressions
cover partial windows, real reliable-work reservations and overflow-adjacent
arithmetic. The UDP/TLS regressions use the original sender's actual opening
window and cover complete admission, priority, expired sends and prepared-record
refusal. These checks do not establish improved throughput on an impaired path;
the wider `fr-xfvs` qualification remains open.
