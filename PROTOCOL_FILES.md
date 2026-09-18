# File authorization envelopes

This implements PROTOCOL.md kinds 0x0070 through 0x0074, without an ATP codec or
runtime dependency in fr-wire. The existing FRD0 header and extension rules apply.
All integers are big-endian. The fixed body prefix is session (u128), lease
(u128), approved local handle (u128), and transfer ID (u64). None is a bearer
credential: callers must supply the authenticated channel, direction and role.
Observers cannot use this lane. Complete record size F cannot exceed selected C.

Each variable ATP payload is prefixed by its u32 byte length. FileOffer adds a
u16 profile and its ObjectManifest. FileAccept adds profile (u16), size (u64),
bytes/second (u32), maximum data chunk (u32), concurrency (u16, exactly one),
and ObjectRequest. FileChunk carries the bounded ATP payload. FileComplete adds
publication disposition (u8), reason (u16), published bytes (u64), and ATP proof.
FileCancel adds a nonzero u16 reason. Complete encoding distinguishes durable
publication, publication with uncertain durability, refusal, and unknown effect;
a refusal cannot contain published bytes or a success proof.

Profile 1 names the pinned Asupersync 0.5.0 portable single-regular-file full
object subset. The runtime adapter must validate the inner ATP operation and
integrity before emitting success. The envelope does not enable resumption,
directory synchronization, file access, or a new network listener by itself.

Six new tests independently construct bytes for all five kinds, exhaust every
truncation point, and exercise binding/direction/handle refusal, complete-record
budgets, forged lengths and contradictory receipts. Source Cargo tests and strict
Clippy pass for fr-wire; this is not independent wire interoperability.
