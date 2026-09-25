# Refusal records (v0)

Exact implementation contract for `Refused` (`0x0004`) in
[PROTOCOL.md](PROTOCOL.md), bead `fr-rc-protocol-refusal-closure-5dx`.
This document does not qualify transport delivery or any platform capability.

The existing 24-byte FRD0 header is unchanged. A refusal travels on the
existing offending reliable channel, in either direction. It is never a
media/input datagram. Initial negotiation uses binding zero; established
channels require their exact installed binding. No refusal creates authority,
acknowledges cleanup, retries an action, or substitutes for `InputResult`.

The fixed payload, before optional extensions, is:

| Field | Encoding |
|---|---|
| reason | `u16`, one of the assignments below |
| operation | `u8` presence (0/1), followed by `u64` only when present |
| effect stage | `u8` presence (0/1), followed by `u8` only when present |

Stages reuse the input receipt assignments: 0 admitted, 1 submitted to OS,
2 observed. A stage requires an operation reference and a nonzero binding.
Initial negotiation cannot report OS effects. The operation is only an opaque
numeric reference, never a ticket, lease credential, path, or reflected text.
Unknown reasons/stages/presence values refuse. A record with no optionals is
28 bytes; with both optionals it is 37 bytes, excluding optional extensions.
The ordinary negotiated complete-record ceiling still includes extensions.

Reason assignments are independent of the input-receipt reason namespace:
1 invalid message; 2 unsupported version; 3 unsupported profile;
4 required capability unavailable; 5 invalid limits; 6 invalid selection;
7 permission denied; 8 local approval denied; 9 approval expired;
10 control unavailable; 11 resource limit; 12 invalid state; 13 expired;
14 host unavailable; 15 tailnet membership unverifiable.

## Independent fixtures

These bytes are written from the layout, not captured from an encoder.

Initial control unavailable (binding 0, no operation or stage):

```text
46 52 44 30 00 00 00 04 00 00 00 00 00 00 00 04
00 00 00 00 00 00 00 00 00 0a 00 00
```

Expired operation 9, submitted to OS, on binding 7:

```text
46 52 44 30 00 00 00 04 00 00 00 00 00 00 00 0d
00 00 00 07 00 00 00 00 00 0d 01 00 00 00 00 00
00 00 09 01 01
```

The wire tests cover exact encode/decode, every truncation and stream split,
invalid flags/enums/presence/lengths, limits, binding mismatch, datagram refusal,
pre-session effect claims, and expiry while a peer trickles a record. Receiving
a valid refusal in the negotiation decoder returns its typed reason as an
error; it never becomes a successful negotiation message.

## Native host startup reporting

The owned `Host::open` path reports compatible pre-observation negotiation
failures to the production viewer, including unavailable control, failed
version/capability negotiation, and local consent denial. It fences session
authority and retires consent before attempting any reporting I/O. Reporting
never invokes admission refresh, capture, input, or another approval callback.

The single refusal uses the original zero-bound reliable control stream. A
100-ms outer timer and 16-turn ceiling bound the drain; the original peer proof,
connection lifetime guard and retained-record deadlines may end it sooner.
Backpressure retains the same prepared record, not a repeated additive grant.
No transport guard is disabled to deliver a nicer error message.

Only Hello/Selection/Approval phases may report. After SessionOpened can have
been queued, this path closes without flushing potentially obsolete authority
records. Refusals are not answered with refusals. Closed, cancelled, malformed
transport or revoked-identity paths may be unable to report at all. Absence of
a refusal therefore remains an unknown connection failure, never proof of
clean shutdown. Established-session LeaseRevoked/Closed integration remains
separate; this slice does not implement those message kinds.

The added native startup regressions use real local TLS/QUIC/UDP and the
production viewer with test-only identity admission. They are not a claim
of installed-tailnet, two-machine desktop, or platform qualification.

## Native CLI projection

The native client preserves content-free host startup refusals through connection,
observation and reconnect wrappers. `fr displays` and desktop-enabled `fr connect`
return the existing version-1 refusal envelope with a stable `host_*` error code
and locally authored next-action text. For example, required-capability mismatch
is `host_required_capability_missing`, while denied local approval is
`host_local_approval_denied`; neither is reduced to a generic connection failure.

Only connection-level refusals without an operation/effect receipt use this
projection. Local cancellation and unconfirmed native cleanup retain their own
outcomes and take precedence. Host refusals do not become reconnectable, change
the requested role, or replay approval or input. The shipped-CLI namespace tests
exercise both example reasons over real TLS/UDP with fixture LocalAPI/ingress;
the default and desktop-enabled CLI unit targets cover all reason assignments.
These are implementation tests, not installed-tailnet qualification.
