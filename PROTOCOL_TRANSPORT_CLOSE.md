# Native terminal close fallback (v0)

This extends the registered host terminal-report path described in
[PROTOCOL_CLOSURE.md](PROTOCOL_CLOSURE.md) and
[PROTOCOL_LEASE_REVOKED.md](PROTOCOL_LEASE_REVOKED.md). It does not replace either
FRD0 report, and does not provide a new post-fence application send API.

## Scope and ownership

When a registered `ClosedReport` or `RevocationReport` cannot prepare its reliable
report because native payload, retransmission, or partial-record state remains,
the original fenced connection may instead send one protected QUIC application
`CONNECTION_CLOSE` (frame type 0x1d). The pinned native close operation enters
Draining before flushing; it discards pending protected application packets and
emits only the close. Do not flush application data to make room for a report.
Unstaged application data still follows the existing discard policy.

The local authority fence runs before terminal custody is captured. Only the
original socket and immutable terminal destination/ingress guard are retained.
The independent cleanup context's cancellation and the original 250 ms absolute
closure deadline apply on every asynchronous poll. There is no retry loop,
ordinary receive callback, new connection, deadline extension, or I/O in Drop.
Already transmitted application packets cannot be retracted.

The immediate `close_with_closed` / `close_with_revocation` APIs and the client
CloseRequest exchange retain their existing native-backlog refusal. This fallback
is confined to already-registered host teardown. A missing registration, abandoned
in-flight socket, or dropped report consumer does not acquire another socket.

## Application error codes

The reason phrase is empty. Codes carry only the original owner's typed reason:

| Family | Application error code | Valid reason values |
|---|---|---|
| Session Closed | `0x4652_0100 + reason` | ClosedReason 1 through 8, as in PROTOCOL_CLOSURE.md |
| Lease revoked | `0x4652_0200 + reason` | LeaseRevoked Reason 1 through 9, as in PROTOCOL_LEASE_REVOKED.md |

For example, host failure at session scope is `0x4652_0107`, host stopping is
`0x4652_0102`, and local input revocation is `0x4652_0201`. Zero/reserved reasons
and other codes are not recognized reasons. Interpret these only from an
application close on the original authenticated 1-RTT connection, never from an
Initial-space close, a transport error code, an address, or an unauthenticated
packet. Unknown codes retain generic transport-closure semantics.

A `ClosedReport` consumer supplies its actual final reason after teardown. The
private construction-time HostFailure placeholder must not escape in a packet.
Changing the final reason does not restart the original closure deadline.

## Evidence and results

A close packet is not a Closed or LeaseRevoked report: it carries no remote
session/lease identifier, cleanup stage, receipt count, or external-effect result.
Its interpretation is restricted to the original connection. Even a successfully
flushed fallback returns the original report's `Backpressure` refusal, not `Ok`.
A socket send is neither transport acknowledgement nor peer receipt. Missing
cleanup/effect information remains unknown and original receipts remain intact.

`crates/fr-transport/tests/terminal_close_packet.rs` exercises real localhost
TLS/UDP, queued native video, unacknowledged stream data, final reason replacement,
application-peer closure, cancellation, security loss, delayed first poll and
abandonment. Admission/fencing are explicit fixtures, not native-input or tailnet
qualification. These tests were added without an available local Rust toolchain;
execution remains unverified in the authoring environment.

## Receiving side (2026-10-13)

`fr_transport::quic::PeerClose` decodes a code into its reason and is also the
sender's encoder, so both sides share one table. When an established
`QuicRecords` finds that its peer closed the connection, it returns
`Error::PeerClosed(reason)` instead of `Error::Closed` only if
`close_was_peer_initiated()` holds and the code is an assigned reason in either
family. Every other close (local close, idle expiry, unknown or zero codes, and
all QUIC transport error codes, which lie far below these values) stays
`Error::Closed`. The pinned API does not expose whether the frame was 0x1c or
0x1d; the value ranges cannot overlap the defined transport codes.

`frd::native_connection::reconnect::peer_close` extracts the reason through
every session and streaming transport wrapper. It is never retried. `fr`
names it with the same codes as the corresponding report: a session reason
like a `Closed` report (`host_stopping`, `host_failure`, ..., the table in
`fr_cli/closure.rs` from 60158e8), a lease reason like a `LeaseRevoked` report
(`host_control_revoked`, ...). No `host_report` or `revocation` object is
printed, because none arrived; the next action says cleanup and effects are
unknown.

Tests: `peer_close` unit tests (every assigned reason round-trips, nothing
else decodes); `terminal_close_packet` (the application peer now ends with
`PeerClosed(Lease(LocalRevoke))`); frd `recovery_tests` (extraction through
seven wrappers, never retried or relabelled as a local lapse); the CLI's
`a_host_close_code_is_named_like_its_report_without_claiming_one`.

Namespace evidence: before this, two optional impairment rows ended
`native_session_failed` with `Transport(Closed)` once the host began sending
this close (40 ms RTT with 5% loss, controlled; 200 ms RTT, view-only).
Live-tailnet qualification remains under bead `fr-t4lj`.
