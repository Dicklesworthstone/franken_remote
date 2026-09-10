# Native media attachment records

`fr-wire::attachment` implements the native-media-attachment version 1 capability.
It is explicitly negotiated on an already admitted native QUIC session. It adds
no identity mode, listener, origin exception, input grant or decoder readiness.
The full session, display, geometry, configuration, recovery and viewport tuple
is immutable; the native stream pair is an adapter allocation, not an assignment
of fixed stream numbers to application roles. Media roles are configuration (1),
recovery (2), and video progress/repair (3), with primary data host-to-viewer.

All integers are big-endian. The descriptor is 118 bytes: channel binding u32;
host boot, OS session, remote session and display handle (four u128); geometry,
configuration, recovery and viewport (four u64); role u8; primary direction u8
(fixed 1); host and viewer native unidirectional stream IDs (two u64). The IDs
must fit QUIC's 62-bit space and name the indicated initiator and direction.

A grant is that descriptor plus an opaque 128-bit one-use ticket, host-monotonic
expiry u64, byte allowance u64, picture allowance u32 and credit epoch u64.
The initial attachment reserves bounded reliable byte records, not permission
to submit pictures: picture allowance is zero. The existing decoder and media
resource owners must separately admit configuration, pictures and datagrams.
The byte allowance is positive and no larger than selected control-message C.
A ticket or compact binding alone never supplies observation authority.

| Kind | FRD0 header binding | Payload |
|---|---|---|
| StreamBinding 0x001b | Established control | Descriptor (118 bytes) |
| BindingAccepted 0x001c | Established control | Offered channel binding (u32) |
| ChannelTicket 0x0018 | Established control | Grant (162 bytes) |
| ChannelAttach 0x0019 | Offered auxiliary | Same grant (162 bytes) |
| ChannelAttached 0x001a | Offered auxiliary | Same grant (162 bytes) |

Unlike the initial-control acknowledgement described in PROTOCOL_NEGOTIATION.md,
the later BindingAccepted is carried on the already bound control stream and
names the auxiliary binding in its payload. Only startup changes that stream's
own binding. The two contexts have separate codecs and state machines.

The complete fixed records are 142, 28 and 186 bytes respectively, including the
24-byte FRD0 header. Every record respects C; lists and variable strings are
absent. Unknown direction, role, stream type, parent scope, zero identifiers,
zero tokens/deadlines/credit epochs and oversized allowance refuse. Typed
initial generation zero is valid and is never interpreted as absent. Existing
optional/required extension validation remains in force within the fixed cap.

BindingAccepted and ChannelAttach travel viewer-to-host. The other records
travel host-to-viewer. None is a datagram or a zero-binding bootstrap message.
Secret tickets, session identities and native stream numbers are omitted from
diagnostic formatting.

The codec does not implement replay prevention, current generation selection,
atomic consumption, actual resource reservation or native route installation.
Those belong to the enclosing connection/attachment owner. In particular,
receiving a descriptor is not permission to open a decoder. The owner must check
its live admitted session and selected capability, consume a matching ticket
once, preserve original deadlines under backpressure, require the attachment
acknowledgement before application data, and retain retired binding identities.

Tests independently assemble all five golden layouts, exercise every truncation,
role/delivery/parent/stream substitutions, limits, output capacity, zero-binding
preallocation rejection and redacted diagnostics. These are source/codec tests,
not live-tailnet or independent-peer qualification. Reproduce with:

```sh
cargo test -p fr-wire --test attachment --locked
cargo clippy -p fr-wire --all-targets --locked -- -D warnings
```

Related contracts: [PROTOCOL.md](PROTOCOL.md),
[PROTOCOL_NEGOTIATION.md](PROTOCOL_NEGOTIATION.md),
[PROTOCOL_DECODER.md](PROTOCOL_DECODER.md).

The configuration-role runtime join is implemented and tested in
[NATIVE_MEDIA_ATTACHMENT.md](NATIVE_MEDIA_ATTACHMENT.md); other roles still
require separate runtime implementations.
