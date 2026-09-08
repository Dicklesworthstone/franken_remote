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

HeldState remains specified but unimplemented. InputResult uses the separate
receipt payload below. Unknown kinds are refused rather than claimed implemented.
Wire validity alone is not proof of input authorization, native injection, live
QUIC interoperability, or a usable controlled desktop.

## InputResult (0x0048)

InputResult travels reliably from host to viewer on the attached input channel.
It has a 50-byte fixed payload (74 bytes including FRD0), followed by optional
bounded extensions. This assigns the previously unimplemented v0 kind; existing
action fixtures and the application version are unchanged.

| Payload offset | Width | Field |
|---:|---:|---|
| 0 | 16 | RemoteSessionId |
| 16 | 16 | InputLeaseId |
| 32 | 8 | Sequence |
| 40 | 1 | Sequence space: 0 reliable action, 1 absolute pointer |
| 41 | 1 | Stage: 0 admitted, 1 submitted to OS, 2 observed through instrumentation |
| 42 | 1 | Outcome: 0 submitted, 1 applied locally, 2 rejected before submission, 3 expired before submission, 4 cancelled before submission, 5 partially submitted, 6 effect unknown |
| 43 | 4 | Confirmed prefix, in native operations (not bytes, clicks or text characters) |
| 47 | 1 | Unknown next native operation: boolean |
| 48 | 2 | Refusal code; 0 means absent |

The receiver supplies the authenticated channel/session/lease binding and checks
all three. Separate sequence spaces prevent a pointer receipt from acknowledging
an unrelated reliable action with the same sequence. These checks are parse and
binding validation; the caller still owns live-session or bounded closing-drain
admission. A result never authorizes input or retries an action.

Submitted and partially submitted outcomes require a nonzero confirmed prefix.
Applied-local and before-submission outcomes require zero. Only a fully submitted
outcome may report observed; conversion from a core receipt never invents that
stage. Without a confirmed prefix the stage is admitted, even if entry into the
next native call left an unknown effect. Partial/unknown receipts retain every
confirmed operation. The unknown boolean is true exactly for effect-unknown;
the next operation is uncertain and subsequent operations were not attempted.
The bound is the existing 4096-byte committed-text ceiling (at most 4096 scalar
operations), including the uncertain operation. A client interprets the prefix
against its retained original action; the result carries no input content.
Partial actions must leave room for the refused operation. Pointer results have
at most one operation and cannot report partial submission or local application.

Successful/local outcomes have no refusal. Expired outcomes name observation,
lease or ticket expiry; cancellation names local revoke. Rejected/partial outcomes
require a reason, and effect-unknown requires code 32. Undefined codes and
contradictory stage/outcome/count/reason combinations fail closed.

Refusal codes are content-free categories, with no internal phase, sequence-gap
detail, library string, ticket or input payload copied onto the wire:

| Code | Category | Code | Category |
|---:|---|---:|---|
| 1 | invalid_state | 18 | previous_action_pending |
| 2 | observation_expired | 19 | sequence_gap |
| 3 | no_lease | 20 | not_pending |
| 4 | stale_lease | 21 | stale_session |
| 5 | lease_expired | 22 | stale_view |
| 6 | view_unready | 23 | out_of_bounds |
| 7 | controller_busy | 24 | unsupported |
| 8 | controller_cleanup_required | 25 | invalid_transition |
| 9 | ticket_invalid | 26 | relative_overflow |
| 10 | ticket_expired | 27 | mode_mismatch |
| 11 | challenge_mismatch | 28 | revoked |
| 12 | challenge_pending | 29 | permission_missing |
| 13 | challenge_expired | 30 | platform_geometry_changed |
| 14 | clock_regression | 31 | platform_unavailable |
| 15 | deadline_overflow | 32 | unknown_effect |
| 16 | invalid_receipt_capacity | 33 | authority_unavailable |
| 17 | sequence_fenced | | |

`InputResult::from_receipt` converts only a completed core receipt. It does not
turn `ConsumedWithoutReceipt`, obsolete pointer state, or a missing process reply
into a new zero-prefix rejection. Those paths still need their own bounded
session/refusal reply integration. Cleanup releases never alter a prior receipt.

## Final submission owner

`fr-core::input_submission::InputSession` now joins these records to the existing
session authority and replay ledger. The local caller transfers one already
admitted authority into it, supplies the selected display bounds and qualified
sink capabilities, and routes parsed actions through `dispatch`. There is no
network listener or implicit identity grant in this API.

Every individual OS operation runs platform preflight, then checks the actual
host clock, current ticket, observation/control, complete view binding and local
revoke immediately before submission. A click's position and button transition
are separate checked operations; committed text is checked at each Unicode scalar.
Results retain the count of confirmed native operations even when the suffix was
expired, rejected or uncertain. An input-mode change or zero relative delta is
`AppliedLocally`, not an invented OS submission. Mode changes require a new ticket
so old absolute-pointer datagrams cannot survive a mode transition.

The owner has fixed held-key/button arrays and a 32-receipt window independent of
the consumed sequence floor. Old pointer datagrams at or below a click barrier
are obsolete; missing pointer sequences cannot gap the reliable action stream.
Partial/refused/uncertain reliable actions fence dependent input; cleanup is a
separate local release-only operation. An unknown press remains tracked before
native entry, including across an unwinding panic. Unknown/failed releases stay
tracked for subsequent cleanup rather than being reported released.

The independent local revoke handle needs no media/authority mutex. It stops
subsequent submissions but cannot undo a native call already entered. The runtime
must service `maintain` while idle and connect its independent watchdog to revoke;
this synchronous core starts no timer. Focus loss, suspend and view replacement
end this input owner and require a new grant, never silent authority restoration.
The sink must not enqueue delayed work or retry behind the final check. Native
permission/geometry checks and physical/synthetic key collisions remain platform
responsibilities. Tests using the recording fault sink prove policy behavior,
not native OS effects or a complete interactive remote desktop.
