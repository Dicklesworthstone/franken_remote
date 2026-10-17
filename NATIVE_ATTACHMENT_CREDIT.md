# Native attachment credit v1

This optional native-profile extension implements the downward endpoint-budget
negotiation required by plan §17.2, within the existing attachment exchange.
It does not add a channel, round trip, queue, runtime or authority source.

## Wire contract

Both peers select `native-attachment-credit` version 1 alongside
`native-media-attachment` version 1. It applies to each attached role, including
observation media; role-specific capability and authority gates are unchanged.
Capability absence or an incompatible optional version keeps the legacy exchange.
A large generic ProtocolLimits value is not evidence of this extension.

Only with that positive selection, the viewer sends BindingAccepted (FRD0 kind
0x001c) on the original reliable control stream with this exact body:

| Field | Width | Meaning |
|---|---|---|
| binding | u32 big-endian | The offered channel, not the control binding |
| byte_allowance | u64 big-endian | Viewer's local maximum record for this role |

The body is exactly 12 bytes (36 including the existing FRD0 header), compared
with the legacy 4-byte body. No optional trailing fields are accepted. The
viewer-to-host direction and original control binding are required. Allowance
must be at least the 186-byte channel grant record and no greater than the
selected protocol record ceiling. The ordinary attachment codec still rejects
this extended layout; the extended codec rejects the legacy layout.

The viewer reserves its pair using its existing local role calculation before
accepting. The host intersects the acceptance with its own reservation:
`grant.byte_allowance = min(host_allowance, viewer_allowance)`. Only then does it
issue the one-use Ticket. That exact narrowed Grant is echoed in Attach and
Attached. Ticket identity, deadline, credit epoch, picture allowance, streams,
view generations and authority are unchanged. Another binding's acceptance,
a duplicate/out-of-phase acceptance or a different subsequent Grant refuses.

The final routes, framing and datagram record bounds use the narrowed grant.
The original reservation remains bounded by local policy. Lowering a record
limit does not retract native stream credit already advertised, enlarge an
aggregate buffer, extend a deadline or authorize a larger application item.

## Independent capabilities

`native-bulk-records` controls whether clipboard/file maxima derive from their
bulk pool or retain the legacy critical-sized ceiling. Attachment credit only
intersects the two endpoints' resulting role maxima. Neither extension selects
clipboard, files, audio or input by itself. A controller with 8 KiB of local
bulk storage can agree with a 64 KiB host without accepting a 64 KiB record.
A smaller input, video or audio allowance likewise cannot enlarge another role.

## Codec checkpoint

The wire codec and immutable grant intersection pass six dedicated tests,
including an independently written 36-byte fixture, every stream split and
truncation, both codec downgrade directions, all seven roles with asymmetric
allowances, malformed grants and bilateral version selection. They run within
458 passing fr-core/fr-wire tests (zero failures/ignored) on the pinned
nightly-2026-08-31. All-target fr-wire Clippy with warnings denied passes.

This checkpoint does not advertise the capability or change native attachment
orchestration yet. Those changes require wiring the same selected decoder into
the attachment owner and the shared clipboard/file dispatcher. A codec test is
not native transport, live-tailnet or device qualification. Tests used verified
retained c45b547 sources plus the exact previously published bulk-record files;
the edited attachment baseline matches main 9805165. This is a scoped component
check, not a complete current-main workspace build.
