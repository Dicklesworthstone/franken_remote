# Native session negotiation (v0)

This implements PROTOCOL.md sections 1–4 and the six startup kinds below. It is
not Tailscale identity, local approval, a codec probe, a control lease, browser
bootstrap, or a frozen interoperability specification. Native profile ID 1,
profile version 0, and application version 0 are independent values. Other
transport profiles explicitly refuse; no missing-Origin authentication exists.

## Bounded encoding

All records use the existing 24-byte FRD0 header and extension validation. The
complete record is capped at the smaller of the caller's limit and 4,096 bytes.
Initial messages have binding zero. BindingAccepted alone has the installed
nonzero binding in both header and payload. No initial-message exception applies
to input or media. `RecordStream::negotiation` is an explicit constructor;
`RecordStream::new` still refuses zero. The initial framer checks the six-kind
profile (five zero-bound kinds) before allocating an announced payload.

Integers are big-endian. `limits` is C:u32, T:u32, A:u32, X:u32, P:u64, W:u8,
B:u64, exactly the 33 bytes and field order from PROTOCOL.md section 3. Decoding
uses the canonical LimitOverrides validation, not duplicate resource ceilings.
This startup subset additionally requires C >= 128. Each actual reply must still
fit its negotiated C: a tiny offer cannot force a larger SessionOpened.

`capabilities` is count:u32 followed by entries
(name_length:u32, name:UTF-8 bytes, version:u16, required:u8). At most 16 entries;
names are 1–64 ASCII lowercase letters, digits, dot, hyphen or underscore,
strictly lexicographically increasing and unique; version is nonzero; required
is 0 or 1. Unknown names remain data until negotiation, not executable features.
There are no nested capabilities or unbounded extension objects. Offered app
versions are count:u32 plus at most eight strictly increasing u16 values.

`role` is Observe=0 or RequestControl=1. RequestControl expresses intent only:
it neither reserves a seat nor creates an input lease, readiness or ticket.

| Kind | Fixed payload before extensions |
|---|---|
| ClientHello 0x0001 | versions, profile:u8, profile_version:u16, role:u8, limits, capabilities, nonce_present:u8=0 |
| HostCapabilities 0x0002 | same offer layout; implemented intersection contains only app version 0; nonce_present=0 |
| SelectedConfiguration 0x0003 | version:u16, profile:u8, profile_version:u16, role:u8, limits, capabilities |
| ApprovalRequired 0x0010 | request_remote_session:16 bytes, host_deadline_us:u64, role:u8 |
| SessionOpened 0x0011 | host_boot:16, os_session:16, remote_session:16, control_binding:u32, six absent-option bytes, channel:u8=1, direction:u8=0, selected configuration, observation_until_us:u64 |
| BindingAccepted 0x001c | binding:u32, equal to the nonzero record-header binding |

For SessionOpened, the six absent options explicitly represent display identity,
geometry, codec configuration, recovery, viewport mapping and input lease in
that order. Channel 1 means connection control; direction 0 means bidirectional
application control. The two transport streams can remain unidirectional. No
absent field is a zero-valued generation wildcard. Display/media/input binding
tuples require their separate later binding codecs; this initial tuple cannot
be used as their authority. Core opaque IDs retain their full 128 bits.

The initial-control binding ID is a connection-local routing label, not a
credential. The host must allocate fresh opaque session IDs with qualified
randomness. This codec deliberately does not create randomness or authentication.

## Selection

The host intersects both validated offers with implemented application version
0. Capabilities intersect by exact name/version. A required capability missing
from either side refuses; when present its required flag is the union of both
requirements. Optional unsupported capabilities are omitted. Selected limits
are fieldwise no greater than either offer and satisfy all canonical cross-field
constraints. A selection can omit optional capabilities, but cannot add new
ones, change versions, erase required flags, silently change role, or raise a
limit. The viewer independently checks HostCapabilities against its original
offer, and SessionOpened must acknowledge its exact selection.

Capabilities must be advertised by the qualified owning implementation. Merely
encoding a name here is not proof that a codec, transport or input adapter works.
Diagnostic formatting omits capability names and control-binding identities.

## Evidence and remaining joins

`cargo test -p fr-wire --locked` exercises the new codecs and existing input/media
codecs. `cargo clippy -p fr-wire --all-targets --locked -- -D warnings` checks the
same source. The startup tests include independently assembled hello bytes,
every truncation of all six messages, invalid collections/profile/limit offers,
required-capability negotiation, exact output capacity, all incremental hello
splits, exclusive framer expiry, extension rules and pre-allocation rejection of
zero-bound media/input. These are protocol-source tests, not live QUIC or a
qualified network-facing daemon. Runtime session composition and the listener
must enforce identity, local approval, sequencing, timeouts and teardown.
