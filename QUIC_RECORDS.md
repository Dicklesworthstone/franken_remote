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

During async I/O the connection is moved out of the owner. Dropping the drive
future drops that connection rather than allowing unsafe continuation after
partial effects. Completed or cancelled work cannot be replayed on a new
connection. The parent still performs its normal input revocation and cleanup.
A synchronous receive handler must not block on capture, decoding, presentation,
or input injection; these already have separately owned native workers.

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
budget, which starts at the first fragment. Likewise one `receive` turn drains
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
The pinned API has no public retained-buffer count: this adapter saves an EMPTY
outbound `QuicStream` value before its first write, refreshes only its public
scalar counters, and uses that type's structural equality to prove the private
pending and retained maps are empty. It never clones live payload maps or parses
Debug output. Any unmatched private state conservatively prevents release. This
version-specific absence proof must be requalified on an Asupersync upgrade and
can be replaced by an explicit upstream retention query when available.

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
stream keep bounded acknowledgment epochs (the absence proof above). Critical
streams freeze their membership at the first native prefix. Bulk streams now
allow newly admitted records to join for a fixed 10 ms from that first prefix,
so a producer serviced on successive turns can fill the existing budget without
waiting an extra RTT for every record. The first prefix is not delayed. Joining
is decided at admission, even if congestion prevents a native prefix until later.
After the fixed window, new records wait for the next epoch; continued traffic
never slides the window. All original byte/count limits, critical connection
credit, per-stream FIFO order and delivery deadlines remain unchanged. No byte
is released until the entire current epoch is proven absent from both native
queues. An empty epoch retires immediately, including inside its fill window.
Per-record acknowledgment and a fully sliding send window still require an
explicit upstream retention query.

The batching policy's five direct-source unit tests and strict Clippy check
pass on the pinned nightly. Reverting to always-closed batches or sliding the
fill start makes four tests fail in each case. `bulk_epochs.rs` adds real
UDP/TLS coverage for staggered FIFO delivery, bounded credit reclamation and
unchanged earliest deadlines. Those transport tests are not yet qualified by
this change: local compilation of the pinned Asupersync dependency was killed
at the container's 4 GiB memory ceiling. No WAN throughput improvement or
clipboard item ceiling is claimed without the corresponding live runs.

Delivery allowance (2026-09-30). A reliable record's send-by is its usefulness
deadline, enforced at admission: a record already past it is refused. Until
2026-09-30 the send-by was also the deadline for staging and acknowledgement,
and missing it closed the connection. A 250 ms record on a 120 ms RTT path
waits one round trip for the previous epoch and another for its own flight and
ACK. Any loss recovery comes on top of that. In namespace runs at 60 ms one-way
delay, control and view-only sessions ended with `transport_deadline_expired`
while both peers were healthy. An admitted reliable record must now be staged
and acknowledged by max(send-by, admission + `delivery_allowance_us`):

- The allowance is seven RFC 9002 probe timeouts, 7 x (smoothed RTT +
  max(4 x RTT variation, 1 ms) + 25 ms maximum ACK delay): the span of three
  consecutive backed-off probes (1 + 2 + 4). It was three until 2026-10-13
  (below).
- It is clamped to [250 ms, 2 s].
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
At 40 ms RTT the allowance is now about 480 ms; at 120 ms about 1 s; 200 ms
RTT reaches the 2 s ceiling. A silent peer still closes the connection within
2 s, and content lateness is still enforced where the content is used. In one
controlled matrix run each, the lossy 40 and 120 ms rows held 29 and 32 steps
in total against 24 before, and the 40 ms, 1%, 5 Mbit/s row held. Most
remaining lossy-row ends are not this deadline (fr-xfvs notes).

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
