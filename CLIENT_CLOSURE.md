# Closing-only client session exchange

`QuicRecords::close_with_request` closes ordinary I/O synchronously and transfers
only its original socket, inbound control parser/remainder and one fixed
CloseRequest into a closing exchange. Unstaged application writes are discarded.
Native retained/retransmission payloads, partially staged writes and queued
outbound datagrams refuse instead of being flushed after the application fence.
The caller stops observation/input admission first; this API is not native input
cleanup and does not authorize a reconnect or replay.

Only the original reliable control pair is read. A partially received record
continues with its existing framing and deadline, not a new parser in the middle
of a record. At most 32 control records are examined; stale control messages have
no application callback or renewal response. A valid Closed report stops parsing
immediately, including when another record follows it in the same native read.
Configured receive bounds are unchanged. New streams, invalid framing, a wrong
session, cancellation, expired security evidence or the immutable deadline end
the exchange. The original destination/ingress check is never replaced.

The maximum 250-ms budget starts at construction; a caller's earlier deadline can
only shorten it. Request transmission, waiting for the report and flushing its
transport ACK share that budget. `CloseOutcome` separates request transport ACK,
the exact optional host report, and the exchange result. A report remains retained
even if its ACK flush fails. No report is Unknown, not zero outstanding actions;
transport success cannot upgrade cleanup/effect stages or erase action receipts.

Ten new production TLS/UDP tests cover exact uncertainty, native backlog refusal,
unstaged-write discard, partial inbound records, fixed deadlines, cancellation,
foreign connections, malformed reports, bounded control floods and ignoring every
record after Closed. All 42 terminal tests (ten new plus 32 unchanged prior tests)
pass. Strict pedantic Clippy passes for the transport library and terminal test
target; changed-source formatting and whitespace checks pass. Tests use explicit
session/cleanup fixtures, not installed-Tailscale or native input-release evidence.

The build uses pinned nightly-2026-08-31 and source-rebuilt first-party libraries
from bd177a8 (Rust sources match main 620b8305), with unchanged compiler/lock-matched
external libraries retained by CI run 36226003278. Archive checksums were checked.
This is not a cold dependency build or complete current-workspace qualification.
This transport slice does not yet wire the running viewer or native window close;
control-capable shutdown and confirmed native cleanup remain separate obligations.
Ref: fr-rc-protocol-refusal-closure-5dx; plan 19 and PROTOCOL.md sections 4/6.
