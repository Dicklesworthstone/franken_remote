# Native Asupersync QUIC record transport

`fr-transport::quic::QuicRecords` connects FRD0 records to the real
`NativeQuicUdpConnection` in the workspace-pinned Asupersync 0.4.10. QUIC is
primary. No WSS implementation, alternate QUIC stack, executor, public listener,
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
storage, four pending outgoing datagrams, and two-second partial-record lifetime.
The explicit receive-framer capacity and retained read remainders are reported
separately. Native packet/reassembly metadata and Asupersync's bounded incoming
datagram queue are additional resources; the containing broker must account for
them as well as TLS, media caches and decoder surfaces. These are not process-RSS
measurements or a claim that all native allocator overhead fits the payload cap.

Reliable bytes stay charged after native packet assembly. Merely draining
`pending_stream_data_bytes` is not evidence that retransmission copies died.
The pinned API has no public retained-buffer count: this adapter saves an EMPTY
outbound `QuicStream` value before its first write, refreshes only its public
scalar counters, and uses that type's structural equality to prove the private
pending and retained maps are empty. It never clones live payload maps or parses
Debug output. Any unmatched private state conservatively prevents release. This
version-specific absence proof must be requalified on an Asupersync upgrade and
can be replaced by an explicit upstream retention query when available.

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
