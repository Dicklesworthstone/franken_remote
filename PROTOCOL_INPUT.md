# Executable v0 input records

This is the implemented payload layout for the input kinds already assigned in
[PROTOCOL.md](PROTOCOL.md), using its unchanged 24-byte FRD0 record header.
All integers are big-endian. Signed integers use two's-complement encoding.
No new protocol version or alternate transport is introduced. The checked-in
`crates/fr-wire/tests/fixtures/input/*.hex` fixtures were constructed separately
from the Rust serializer and are exact reference bytes, not regenerated outputs.

Input channels must already be attached to an authenticated remote session.
The caller supplies the authenticated sender direction; host-to-viewer input
commands are rejected. Only PointerState may use datagrams, which also require
the transport's negotiated datagram-size check. All other actions are reliable.
The codec checks the negotiated control-record ceiling before parsing and makes
no heap allocation. Text is borrowed from the record, never copied into diagnostics.

## Common payload prefix (88 bytes)

| Offset | Width | Field |
|---:|---:|---|
| 0 | 16 | RemoteSessionId |
| 16 | 16 | InputLeaseId |
| 32 | 16 | InputTicketId |
| 48 | 8 | DisplayGeometryGeneration |
| 56 | 8 | ViewportMappingGeneration |
| 64 | 8 | CodecConfigurationGeneration |
| 72 | 8 | RecoveryGeneration |
| 80 | 8 | Sequence, in the reliable-action or separate pointer space |

These full identities supplement the compact channel binding; neither the
binding nor possession of the identities grants authority. The submission owner
must match all of them and check the current host clock, authority, readiness,
focus and input mode immediately before each OS submission.

## Per-kind body, immediately after the common prefix

| Kind | Body |
|---|---|
| 0x0040 KeyTransition | Physical keyboard usage u16; transition u8 (0 release, 1 press, 2 client-owned repeat) |
| 0x0041 ButtonTransition | Button u8 (1 primary, 2 secondary, 3 middle, 4 back, 5 forward); pressed boolean u8; x/y i32; pointer barrier u64 |
| 0x0042 PointerState | x/y i32 |
| 0x0043 RelativeCheckpoint | Input-mode epoch u64; cumulative x/y i64 |
| 0x0044 Scroll | Target x/y i32; pointer barrier u64; distance x/y i32; units u8 (0 pixels, 1 lines) |
| 0x0045 CommitText | UTF-8 byte count u32; complete nonempty UTF-8, at most 4096 bytes and within the negotiated control ceiling |
| 0x0047 InputMode | Mode u8 (0 absolute, 1 relative); new input-mode epoch u64 |

Physical keys use keyboard/keypad usage page 0x07, with the deliberately bounded
v0 subset 0x04–0xA4 and 0xE0–0xE7. They are not Unicode, keysyms or OS keycodes.
The platform adapter must map physical positions or refuse them. Committed text
is an independent, explicitly qualified capability, never layout guessing or
clipboard substitution. Coordinates are already transformed host desktop pixels;
negative monitor origins are valid and out-of-content positions are refused, not
clamped. Each click/scroll carries its own target and pointer-sequence barrier.

Booleans accept only 0/1; enum values, physical usages, UTF-8 and exact payload
lengths are checked. FRD0 optional extensions follow the existing bounded ordered
extension rule; unknown required fields fail closed. Credentials, key identities,
coordinates and text are omitted from input Debug output.

HeldState and InputResult are still specified but not encoded by this first
input-record slice. Unknown kinds are refused rather than claimed implemented.
Wire validity alone is not proof of input authorization, native injection, live
QUIC interoperability, or a usable controlled desktop.
