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
