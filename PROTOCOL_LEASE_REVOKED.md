# LeaseRevoked (0x0014)

This supplies the v0 byte assignments required by PROTOCOL.md sections 2 and 6
for `fr-rc-protocol-refusal-closure-5dx`. The host sends this bounded notification
on the original reliable session-control stream, after locally fencing the
named input lease. Neither enqueue success nor peer receipt performs revocation.
Observation permission and completed native cleanup are separate facts.

## Encoding

All integers are big-endian. The ordinary 24-byte FRD0 header has kind 0x0014,
a nonzero installed control binding and payload length 36 (60 total bytes without
extensions). The existing bounded, ordered extension rules apply unchanged.
The control binding already identifies the host boot and OS session; the payload
repeats the complete remote-session and input-lease IDs, never compact aliases.

| Payload offset | Bytes | Field |
|---|---:|---|
| 0 | 16 | RemoteSessionId |
| 16 | 16 | InputLeaseId |
| 32 | 2 | Reason |
| 34 | 1 | Cleanup stage |
| 35 | 1 | Effect/receipt stage |

Reason values: 1 `local_revoke`, 2 `lease_expired`, 3 `observation_ended`,
4 `view_invalidated`, 5 `session_ended`, 6 `permission_lost`, 7 `host_failure`,
8 `client_requested`, 9 `suspended`. No free-form diagnostics are allowed.
Unknown reason or stage values refuse; zero IDs are not wildcards.

Cleanup stages: 1 **Fenced**, with native release not certified; 2 **Released**,
with held-input cleanup completed; 3 **Failed**, with input fenced but cleanup
unsuccessful. Every stage requires that new input is already forbidden.

Effect stages: 0 **Unknown**, 1 **ReceiptsPending**, 2 **ReceiptsComplete**.
Complete means individual input receipts account for the accepted actions, not
that external effects have been rolled back or that actions ran exactly once.
No aggregate stage replaces or discards an InputResult, including a partial or
uncertain native effect. Cleanup and receipt completion are independent.

The receiver checks authenticated host-to-viewer direction, reliable delivery,
record and extension limits, and exact current session/channel/lease before
acting. A stale lease report cannot revoke its replacement. A valid report is
terminal for that input owner: queued unsent actions and renewal responses must
not be sent, while already committed action receipts remain reportable. It does
not authorize an automatic control reacquisition. An absent report means closure
status is unknown, never "cleanup succeeded".

## Independent fixture

Control binding 7, remote session 1, lease 2, local revoke, fenced, effects unknown:

```text
46 52 44 30 00 00 00 14 00 00 00 00 00 00 00 24 00 00 00 07 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 01
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 02
00 01 01 00
```

This fixture is hand-written from the table, not dumped from the encoder.
`crates/fr-wire/tests/lease_revoked.rs` checks encoding, decoding, every truncated
prefix, short output buffers, wrong role/channel/session/lease, invalid enums,
record limits, extensions and content-free diagnostics. These are codec tests;
they do not establish hardware behavior or independent network interoperability.
