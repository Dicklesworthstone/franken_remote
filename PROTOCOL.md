# PROTOCOL.md — FrankenRemote Wire Specification

**Status: normative v0-draft, not frozen or implemented.** This is the contract
for bead `fr-fr-wire-framing-i0u`, authored under `fr-protocol-draft-bh9`.
The [design plan](COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md),
especially §§1, 7, 12, 15–17, 19, and 27.1, takes precedence. MUST and MUST NOT
express requirements on an implementation; they do not report tested behavior.
The draft is replaced in place by the qualified specification at the Phase 5
freeze bead `fr-p5-protocol-freeze-e84`. No wire interoperability, TLS, codec,
hardware, or performance qualification follows from this document.

## 1. Transport, identity, and versions

One application protocol runs over three separately versioned profiles:
`native-quic`, `webtransport-h3`, and `wss`. Asupersync is the sole runtime and
shipping transport implementation. QUIC and WebTransport remain subject to
their Phase 0 qualification gates. WSS is a labeled degraded profile.

Before application admission, the host MUST verify the actual ingress and peer
against authenticated installed-Tailscale metadata and the locally selected
sharing scope. Source prefixes, DNS, claimed names, reachability, and email
domains are not authority. Missing membership evidence yields
`tailnet_membership_unverifiable`. The peer binding survives neither a change
of identity nor a reconnect by numeric identifier reuse.

The native QUIC profile requires completed TLS 1.3, verified server certificate
and hostname, and its own ALPN (`fr-remote/0` for this draft). WebTransport uses
`h3` and a separately pinned, qualified HTTP/3/WebTransport revision, with its
real SETTINGS, CONNECT, stream prefixes, datagram association, and control
handling. This document does not assign an invented H3 prefix or certify a
draft revision. **No application operation is admitted in 0-RTT**, including
read-only observation. Certificate expiry never enables skip-verification.

Browser transports and native WSS MUST use the configured exact HTTPS Origin
and authority. Static navigation GET can serve assets without an Origin but
cannot issue authority or start observation. An origin-checked same-origin
HTTPS bootstrap POST issues a short-lived, one-use nonce bound to the verified
peer and requested role. `ClientHello`, the first reliable application record,
consumes it. Null/missing/wrong Origin, unexpected Host, and replayed nonces
refuse attachment. Native QUIC uses its authenticated application handshake;
absence of Origin is never an alternate WSS authentication mode.

Bootstrap replies are non-executable JSON with no-store/nosniff protections.
Nonces and tickets travel only in bounded request/response bodies, never URLs,
query strings, process arguments, logs, or host links. Restrictive CSP,
`frame-ancestors 'none'`, no third-party scripts, and no persistent service-worker
authority apply to the browser shell (plan §16.4).

`ClientHello` offers application versions, capability name/version pairs,
required capabilities, transport profile/version, and receiver limits.
`HostCapabilities` returns the supported intersection and host policy bounds.
`SelectedConfiguration` selects one offered application version and a validated
downward negotiation of limits/capabilities; `SessionOpened` acknowledges that
exact selection. No observations or expensive codec probes precede admission
and any required local approval.

Application version **0**, robot JSON schema version, capability versions, and
transport profile version are independent fields. Only application version 0
is defined here; unknown required capabilities/versions refuse negotiation.
Optional unsupported capabilities are omitted explicitly. Robot JSON is not a
second wire encoding. WebTransport compatibility is recorded independently;
WSS fallback retains all identity, Origin, attachment, limits, and expiry rules.
A profile change requires a newly authenticated bound channel/session transition.

## 2. Framing and primitive representation

Application integers are fixed-width **big-endian**; no application varints or
native struct serialization. Transport-owned QUIC/H3 encodings remain those of
the qualified transport profile. A record starts with this 24-byte header:

| Offset | Width | Field |
|---|---:|---|
| 0 | 4 | Magic, ASCII `FRD0` |
| 4 | 2 | Application version, `u16`, zero for this draft |
| 6 | 2 | Message kind, `u16`, from §6 |
| 8 | 2 | Required flags, `u16`; currently zero only |
| 10 | 2 | Reserved, MUST be zero |
| 12 | 4 | Payload bytes, `u32`, excluding this header, including extensions |
| 16 | 4 | Compact binding ID, `u32`; zero only where explicitly allowed |
| 20 | 4 | Trailing extension bytes, `u32`, included in payload bytes |

The receiver checks `24 + payload_bytes`, `extension_bytes <= payload_bytes`,
kind, flags, role, channel, binding, and the applicable record ceiling before
allocating a payload. Reliable streams carry consecutive complete records;
EOF inside a record is truncation, not a successful close. Each datagram or
WSS binary message carries exactly one record. WebSocket fragmentation is
transport framing and MUST itself be bounded before message assembly. Text
WebSocket messages and trailing bytes outside the declared record are refused.

Fixed payload schemas use `u8/u16/u32/u64`, signed `i32/i64` in two's complement,
and fixed-size opaque IDs. Length-delimited bytes and UTF-8 text have a `u32`
byte length; collections have a `u32` element count. A boolean is one byte, 0
or 1. An optional value starts with a 0/1 presence byte. No NUL-terminated
strings, implicit alignment, padding, pointers, recursive object graphs, or
floating-point wire values are permitted. Collection sizes and each item's
minimum size MUST fit the remaining record before allocation/iteration.
Payload nesting is at most two collection/record levels below the message;
extensions are flat and contain no nested extension containers.

Host-monotonic timestamps, durations, and uncertainty bounds use `u64`
microseconds; audio sample positions use `u64` sample counts. A timestamp is
interpreted only with its clock domain/host boot. Client timestamps are never
compared directly with host deadlines; clock estimates carry uncertainty and
cannot extend authority. Pixel coordinates/origins are signed `i32`; logical
coordinates and scroll distances use signed 16.16 fixed-point `i32`. Relative
movement totals use `i64`. Width/height/stride and byte counts use `u32` on the
wire and checked widened arithmetic locally; downcasts to `usize` are checked.
Physical keys use a `u16` HID usage-page plus `u16` usage pair, separate from
UTF-8 committed text; only the negotiated key subset may be submitted.

An extension is `tag:u16, flags:u16, length:u32, value:[length]`. Tags MUST be
strictly increasing and unique. Flag bit 0 means required-to-understand; other
bits refuse. Unknown optional extensions may be skipped only after their
complete framing and aggregate budget are checked. Unknown required tags,
unknown message kinds, nonzero required header flags, invalid encodings,
duplicates, and leftover fixed-payload bytes refuse. An extension cannot
override a mandatory field, authority decision, or limit. Version 0 defines
no extension tags yet.

The message tables fix kind IDs, required semantic fields, roles, bounds, and
state rules. Exact per-kind field tag/enum assignments and byte fixtures must
land together with their `fr-wire` codecs before that kind is executable;
they may not be inferred from Rust layout or a generic serialization library.
This is a v0 contract for that implementation, not a claim of a frozen v1 ABI.

## 3. Limits and resource admission

`fr_core::limits::ProtocolLimits` owns these values (bootstrap bead
`fr-ws-bootstrap-fr-core-hr6`, plan §17.2). Parser/FFI adapters MUST consume the
negotiated structure rather than copy constants. The table states the exact
initial maximum values, not a promise that every maximum can be used together.

| Symbol | Limit | Initial maximum |
|---|---|---:|
| C | Ordinary complete record, header and extensions included | 65,536 bytes (64 KiB) |
| T | Complete UTF-8 clipboard item, on its separate channel | 1,048,576 bytes (1 MiB) |
| A | Complete encoded access-unit payload | 16,777,216 bytes (16 MiB) |
| X | Coded width or height, individually | 8,192 pixels |
| P | Coded width × coded height | 16,777,216 pixels |
| W | Incomplete/held picture window per subscription | 2–12 pictures, maximum 12 |
| B | Per-viewer compressed payload **plus metadata** retained | 33,554,432 bytes (32 MiB) |

The symbols map, in order, to `max_control_message_bytes`,
`max_clipboard_item_bytes`, `max_encoded_access_unit_bytes`,
`max_dimension_pixels`, `max_coded_pixels`, `reassembly_window_pictures`, and
`per_viewer_compressed_bytes`. Offers are constructed through `LimitOverrides`
and `ProtocolLimits::with_overrides`; `ProtocolLimits::negotiated` takes the
field-wise minimum of validated offers. Wire limit fields use the accessor's
width: C/T/A/X are `u32`, P/B are `u64`, and W is `u8`.

Endpoint and administrator limits can reduce these ceilings, never raise them
beyond the implementation bounds. A selected limit MUST be valid and no larger
than either endpoint's offer or local policy. W cannot be less than 2 or more
than 12. Positive byte/geometry ceilings, representable framing, and all
cross-field constraints must be validated; an impossible selection is
`invalid_limits`, not an implicit increase. Negotiating A does not reserve W×A.
Each live allocation still needs count **and** byte admission, including
fragment maps, duplicates, bookkeeping, closing generations, and shared-viewer
retention. Shared sender repair-cache bytes and decoded/GPU surface pools have
separate admitted budgets; B is not a GPU-memory allowance.

`D` is the current maximum complete application datagram, calculated from the
qualified API limit and actual inner-path MTU after IP/UDP, protection, QUIC,
and (when applicable) WebTransport overhead. D also cannot exceed C. Do not
use 1200/1280/1350 as an application payload constant or depend on IP
fragmentation. If D cannot fit a message's minimum header/payload, that
datagram capability is unavailable. QUIC does not fragment DATAGRAM frames
for an application ([RFC 9221 §5](https://www.rfc-editor.org/rfc/rfc9221.html#section-5)).

`F` is the admitted ATP file-channel complete-record limit, no larger than C;
payload length is further reduced by its envelope. Recovery IDRs and text
clipboard items are chunked into records <= C; neither creates a large-record
exception on the control channel. A cursor shape is raw RGBA8, with checked
`width * height * 4` and hotspot bounds, and its entire record must fit C.
Codec-configuration records also fit C; parsed dimensions/DPB/surfaces need
separate validation before a system decoder is configured.

Each advertised capability MUST additionally supply bounded local admission
for streams, handshakes, pending approvals, half-attached/closing channels,
repair ranges/retries/rate/horizon, clipboard chunk counts, audio decoded
samples/jitter, input events/receipts, NAL count, and transfer byte/rate/
concurrency. These are not all implemented by the first core slice. No absent
budget means unlimited: the capability stays unavailable until its owning
slice extends the tested structure and validates its actual allocations.
Higher operating points require positive capability qualification.

Before allocation/FFI, use checked addition, multiplication, stride/plane
layout, alignment padding, and integer conversion. P limits coded pixels,
while padded allocation dimensions/stride/reference pools count against the
separate surface budget. Reject wraparound, zero/contradictory sizes, and
invalid crop/stride before allocation. A legitimate oversized keyframe yields
a smaller operating point or `resource_limit`, never a cap bypass.

## 4. Roles, authority, generations, and attachment

Roles are **H** (host) and **V** (viewer). **K** is a V currently holding the
controller's input lease, never a sender-declared privilege. `V→H` includes K
only where the row's authority conditions hold. A host-sent input command is
always a role violation; the client MUST NOT apply it to its own OS.

State predicates are independent:

| Predicate | Meaning |
|---|---|
| N | Transport/peer identified; only bounded negotiation admitted |
| A | Configuration accepted; observation authorized, including any local approval |
| M | Current decoder configuration acknowledged; recovery/decode can proceed |
| R | Current source and presentation readiness established for the target view |
| K | Current control authorization and an unexpired lease; input also requires R and a valid ticket |
| Z | Fenced/closing; no new observation, presses, text, or transfer work |

`SessionOpened` records an admitted observation session, not media readiness or
control. Local approval before A covers pixels, thumbnails, audio, clipboard,
file access, and semantic observation. Pending approval can expose bounded
non-sensitive status only. No received frame or channel attachment grants K.

Host boot, OS session, remote session, input lease, display geometry, codec
configuration, recovery chain, and viewport mapping are distinct typed IDs.
The `fr_core::ids` wire widths are:

| Type | Width / representation |
|---|---|
| `HostBootId` | 16 opaque bytes, big-endian encoding of the core `u128` |
| `OsSessionId` | 16 opaque bytes, big-endian encoding of the core `u128` |
| `RemoteSessionId` | 16 opaque bytes, big-endian encoding of the core `u128` |
| `InputLeaseId` | 16 opaque bytes, big-endian encoding of the core `u128` |
| `DisplayGeometryGeneration` | 8 bytes, `u64` |
| `CodecConfigurationGeneration` | 8 bytes, `u64` |
| `RecoveryGeneration` | 8 bytes, `u64` |
| `ViewportMappingGeneration` | 8 bytes, `u64` |

Opaque identifiers MUST come from qualified randomness and are not credentials.
The core wrapper does not generate or establish randomness. Generation zero is
a valid initial value within a fresh parent epoch, never a wildcard or absence.
Generation counters/sequence numbers never wrap into reuse: exhaustion replaces
or closes the owning epoch. Wire encoding preserves each type's full width;
conversion to a compact ID is an explicit authenticated binding operation.

`StreamBinding` installs an immutable connection-local nonzero `u32` binding
ID for the full tuple: host boot, OS session, remote session, display identity,
geometry, codec configuration, **subscription** recovery generation, viewport
mapping, channel role/direction, and any applicable input lease. Fields that do
not apply are explicitly absent, not zero-valued aliases. `BindingAccepted`
acknowledges installation before dependent datagrams are sent. A codec update,
recovery, viewport change, channel replacement, or lease replacement gets a
new binding ID. IDs are never recycled within the connection; a new connection
has an empty table. Table count/bytes, retired records, and pending acks are
bounded; exhaustion refuses admission rather than retaining an infinite table.
Full generation equality, not numeric stream ID alone, fences worker callbacks.

The OS share session owns capture/shared encoders. Each viewer owns a
subscription and independent recovery state; closing one viewer cannot cancel
another's shared pipeline. A healthy viewer can consume a newly encoded IDR
without restarting its own recovery epoch. Unknown/retired-generation datagrams
are discarded before payload allocation; version 0 has **zero** preconfiguration
media allowance. Reliable use of an unknown binding refuses that channel.

Every auxiliary channel attaches using `ChannelAttach` as its first reliable
record, with one-use opaque ticket, peer/session, direction, role, and binding.
H issues `ChannelTicket` only on the admitted control connection after resource
reservation. H consumes a ticket atomically after peer/role/generation/expiry
checks and returns `ChannelAttached`. No application payload precedes that
acknowledgement. Until attachment the channel may carry only that bounded
exchange; a bare session ID, ticket reuse, or origin-less WSS never suffices.
Ticket material is redacted from all diagnostics. Channel replacement never
implicitly renews observation or control.

## 5. Channel map and ordering

| Channel | Contents and bound |
|---|---|
| control | Reliable ordered negotiation, authority, binding, status; records <= C, reserved count/byte capacity |
| input | Reliable ordered action sequence and results <= C; separately sequenced replaceable pointer datagrams <= D; independent of bulk transfer |
| media-config | Reliable configuration/acknowledgement, cursor shapes, media announcements; <= C |
| recovery | Dedicated reliable chunked IDR; each record <= C, whole AU <= A, charged to B |
| video | Ordinary AU fragments: datagrams <= D, or WSS records <= C under receive credit |
| audio-down / audio-up | Separately enabled Opus, datagrams <= D or credited WSS records <= C |
| clipboard | Reliable begin/chunk/commit, each <= C, complete item <= T |
| files | Reliable bounded ATP envelope, each <= F, independently admitted transfer budgets |

Auxiliary channels all obey §4. Application channel roles do not assign raw
QUIC stream numbers or HTTP/3 prefixes. Native/H3 adapters bind their actual
streams using the qualified transport profile. WSS opens separately bound
control/video/audio connections and bounded auxiliary connections as needed;
any qualified multiplexing over one lower TCP connection must disclose shared
head-of-line blocking. Bulk credit cannot consume the capacity reserved for
revoke/expiry and input. No priority queue is unbounded.

`ChannelAttached` establishes an initial byte/frame allowance L and a credit
epoch. `ReceiverPressure` carries cumulative released-byte/frame counters R,
not repeatable additive grants. The sender tracks cumulative consumption S;
for each dimension, outstanding usage is `S - R` and must stay within L.
Available credit is `L - (S - R)`, with checked arithmetic. Ignore duplicate or
older counters; refuse R > S, epoch mismatch, and counter exhaustion rather
than wrapping. Cumulative counters can exceed L during a long session;
outstanding usage cannot. Return credit only after the corresponding receive
resources are actually freed, including reference retention. The profile must
define the charged allocation cost, including metadata, before sending work;
an AU cannot be admitted by charging only its first small fragment. WSS
`bufferedAmount`, transport ACKs, decode callbacks, and visible presentation
are distinct observations, never interchangeable acknowledgements. A socket
accepting bytes is not permission to exceed receiver credit.

## 6. Message registry

All rows are mandatory-message kinds if enabled; unsupported kinds refuse.
Bounds are **complete record bytes**, including the §2 header. All reliable
messages are also subject to channel count/byte/deadline admission. A semicolon
in a state cell joins requirements, not alternative authorization paths.
Local-only approval, OS submission, and revoke operations are not peer RPCs.
All messages after `SessionOpened` require a live session except bounded
closure/result reporting. Zero binding is allowed only for initial negotiation
(including `ApprovalRequired` and `SessionOpened`), initial attachment, and the
connection-level refusal responding to them. `SessionOpened` carries the full
initial control binding in its payload; the receiver installs it atomically
and sends `BindingAccepted` on that binding before any further session work.

### Negotiation and session

| Kind | Message | Sender / channel | Bound | Required content and state constraints |
|---|---|---|---|---|
| 0x0001 | ClientHello | V→H / control | C | Offered independent versions, required/optional capabilities, receiver limits, requested role, bootstrap nonce when required; first record in N |
| 0x0002 | HostCapabilities | H→V / control | C | Version intersection, limits and non-sensitive capabilities; validated hello in N; no discovery pixels/titles |
| 0x0003 | SelectedConfiguration | V→H / control | C | Exact version/profile/capability/limits selection; after HostCapabilities; downward-only, no codec or approval side effects |
| 0x0004 | Refused | Either / offending reliable channel | C | Typed reason, bounded operation reference, effect stage if applicable; no reflected peer text; before session may use binding zero |
| 0x0010 | ApprovalRequired | H→V / control | C | Pending request ID, host deadline, requested scope; selection valid but A absent; never approval authority |
| 0x0011 | SessionOpened | H→V / control | C | Full host/OS/remote identity, accepted selection, observation deadline, initial control binding; only after A; no lease implied |
| 0x0012 | ControlRequest | V→H / control | C | Requested target/scope or explicit handoff request; A; readiness and old-controller cleanup required before grant |
| 0x0013 | LeaseGranted | H→V / control | C | New input-lease ID, target binding, host deadline, initial sequence; A and R; serialized authority owner has fenced/cleaned old lease |
| 0x0014 | LeaseRevoked | H→V / control | C | Lease, reason, cleanup/effect stage; fence already applied locally, independent of delivery |
| 0x0015 | Challenge | H→V / control | C | Unique challenge, observation/control scope, bound lease if any, fixed host deadline; current authority only |
| 0x0016 | ChallengeResponse | V→H / control | C | Exact outstanding challenge and scope; consumed once before its deadline and existing authority expiry; no resurrection |
| 0x0017 | InputTicket | H→K / input | C | Opaque ticket handle, lease/readiness/geometry/recovery/viewport binding and host deadline; K and R |
| 0x0018 | ChannelTicket | H→V / control | C | One-use ticket, channel role/direction, binding, host deadline; A and required capability/authority; bounded reservation |
| 0x0019 | ChannelAttach | V→H / new channel | C | Ticket plus peer/session/role/direction/binding; only first record; atomic validation/consumption; no sensitive payload |
| 0x001a | ChannelAttached | H→V / new channel | C | Accepted binding and channel budget; valid consumed ticket; attachment does not change authority |
| 0x001b | StreamBinding | H→V / control | C | New compact ID and complete immutable typed tuple; current authorized session; reserve table entry first |
| 0x001c | BindingAccepted | V→H / control | C | Exact offered binding ID; successful bounded installation; not decoder readiness |
| 0x001d | CloseRequest | V→H / control | C | Scope (control release or session close), reason; current session; idempotent, cannot expand authority |
| 0x001e | Closed | H→V / control | C | Final reason, cleanup stage and bounded outstanding-effect summary; Z, after ordered teardown; absence means unknown closure |

Observation/control challenge cadence starts at one second; provisional
deadlines start at three seconds on the host monotonic authority clock. A
response only renews its currently outstanding, unexpired challenge; delayed
responses cannot obtain a fresh lifetime at arrival. Expiry is terminal and
reacquisition requires a new grant, including approval where configured.
Input tickets start in the measured 0.5–1.5 s lifetime range and never outlive
the lease. These are policy starting points, not input latency claims.

### Displays and media

| Kind | Message | Sender / channel | Bound | Required content and state constraints |
|---|---|---|---|---|
| 0x0020 | DisplayCatalog | H→V / control | C | Bounded display entries: OS identity, signed origins, pixel/logical dimensions, scale/rotation, geometry; A and approved disclosure scope |
| 0x0021 | GeometryChanged | H→V / control | C | Display, new geometry and mapping; A; fence old coordinate input before sending |
| 0x0022 | SelectDisplay | V→H / control | C | Requested display and viewport; A; selects own authorized view only; expansion requires approval and resource admission |
| 0x0030 | DecoderConfiguration | H→V / media-config | C | Binding, HEVC codec identifier, exact hvcC, dimensions/crop/color, admitted decoder budgets; A; validate before configure |
| 0x0031 | DecoderConfigured | V→H / media-config | C | Exact configuration/binding, success of API configuration and admitted resources; A; establishes M, not decode/presentation |
| 0x0032 | RecoveryAccessUnit | H→V / recovery | C per chunk; A total | Binding, frame ID, total bytes, offset, capture time, chunk; M; verified IDR, one outstanding recovery per subscription |
| 0x0033 | FirstFrameDecoded | V→H / media-config | C | Configuration/recovery/frame identity, decode milestone and clock scope; M and complete valid IDR decoded; not presentation |
| 0x0034 | AccessUnitFragment | H→V / video | D datagram / C WSS; A total | Immutable binding, frame identity, total/offset/index/count/stride, timestamps, dependency identity, bytes; M; §7 validity required |
| 0x0035 | RepairRequest | V→H / control | C | Current binding/frame and sorted missing ranges; M; admitted retry/rate/byte/horizon bounds |
| 0x0036 | RecoveryRequest | V→H / control | C | Current binding, last useful frame, typed loss/decoder reason; A; coalesced at shared pipeline, no repeated global reset |
| 0x0037 | MediaProgress | H→V / media-config | C | Latest announced frame, dependency/size metadata, source-observation scope/time, pipeline state; A; bounded progress even before idle |
| 0x0038 | CursorShape | H→V / media-config | C | Shape ID, dimensions, checked RGBA8 bytes, hotspot/scale/visibility/composited owner; A; no unknown-shape allocation |
| 0x0039 | CursorPosition | H→V / video | D datagram / C WSS | Shape ID, position, geometry/viewport, sequence; A; confirmed host state only; never an input command |

### Input

| Kind | Message | Sender / channel | Bound | Required content and state constraints |
|---|---|---|---|---|
| 0x0040 | KeyTransition | K→H / input | C | Ticket, action sequence, physical key, press/release/repeat; K and R checked again at OS submission |
| 0x0041 | ButtonTransition | K→H / input | C | Ticket, action sequence, button transition, coordinate, pointer-sequence barrier; K and R; current geometry/mapping |
| 0x0042 | PointerState | K→H / input datagram or input WSS | D datagram / C WSS | Ticket, pointer sequence, absolute coordinate and mapping; K and R; replaceable, reject pre-barrier motion |
| 0x0043 | RelativeCheckpoint | K→H / input | C | Ticket, action sequence, relative-mode epoch, cumulative i64 x/y; K and R; checked difference from last applied totals |
| 0x0044 | Scroll | K→H / input | C | Ticket, action sequence, target coordinate/barrier, signed distances and units; K and R; no guessed unit conversion |
| 0x0045 | CommitText | K→H / input | C | Ticket, action sequence, validated UTF-8 text; K and R; negotiated direct-text capability, bounded complete Unicode units |
| 0x0046 | HeldState | V→H / input | C | Bound lease, reconciliation sequence, bounded physical-key/button set; only current K may reconcile; never creates a press; stale/revoked lease is ignored |
| 0x0047 | InputMode | K→H / input | C | Ticket, action sequence, absolute/relative mode and fresh mode epoch; K and R; ordered barrier, clear prior-mode state |
| 0x0048 | InputResult | H→V / input | C | Lease/action sequence, admitted/submitted/observed stage, outcome, submitted prefix and unknown remainder; live session or bounded Z drain |

One reliable action sequence orders keys, buttons, scroll, text, relative
checkpoints, and mode changes. Absolute pointer sequence numbers are separate;
missing datagrams never create reliable-action gaps. A click/scroll carries
its own coordinate and barrier; later arrival of motion at or below that
barrier cannot move the cursor backward. Relative checkpoint totals are
cumulative within a mode epoch; duplicates cannot apply displacement twice.
Version 0 does not send independently droppable relative deltas.

Immediately before **every OS submission**, the input agent rechecks
authorization, host expiry, ticket, lease, geometry, viewport, recovery/view
readiness, focus, and input mode. No global authority lock spans blocking OS
work. Duplicate action IDs return retained results; a monotonic consumed floor
survives receipt eviction so old IDs yield `unknown_history_no_replay` rather
than another action. Reliable gaps refuse or enter bounded resynchronization;
they never skip an unknown click. Expired presses/mode changes fence queued
dependent actions. Host-owned release-only cleanup remains permitted without
authorizing new presses; a peer cannot use `HeldState` to reacquire control.

The client owns key-repeat event generation in v0; the qualified host adapter
MUST avoid a second synthesized repeat stream or refuse that mode. Text
injection and physical keys are separate capabilities. Unsupported text is
refused, never guessed through a keyboard layout or implicit clipboard paste.
IME composition stays local until committed; reconnect cannot replay it.

### Clipboard, audio, and files

| Kind | Message | Sender / channel | Bound | Required content and state constraints |
|---|---|---|---|---|
| 0x0050 | ClipboardBegin | H↔K / clipboard | C; item <= T | Transfer ID, source label/sequence, total UTF-8 bytes; A and current controlling session, both clipboard enables and OS grant |
| 0x0051 | ClipboardChunk | H↔K / clipboard | C; aggregate <= T | Transfer, increasing chunk index/offset, bytes; accepted Begin, exact declared total, bounded chunk count |
| 0x0052 | ClipboardCommit | H↔K / clipboard | C | Transfer and final total; complete validated UTF-8 and current authority before OS publication; no partial publish |
| 0x0053 | ClipboardCancel | H↔V / clipboard | C | Existing transfer and reason; may cancel in Z; never alters a newer OS clipboard value |
| 0x0060 | AudioConfiguration | H→V (down) / K→H (up), respective audio channel | C | Direction, audio epoch, Opus, 48-kHz mono/stereo, packet duration/decoded-sample/jitter budgets; A; down locally host-enabled; up explicit client talk enable, OS mic grant, qualified host endpoint |
| 0x0061 | AudioConfigured | Receiver→sender / respective audio channel | C | Direction/epoch and accepted budgets; actual configured output; no audio before this acknowledgement |
| 0x0062 | AudioPacket | H→V (down) / K→H (up), respective audio channel | D datagram / C WSS | Direction/epoch, packet sequence, sample position/duration, Opus bytes; enabled acknowledged configuration, current authority; validate decoded samples before allocation |
| 0x0063 | AudioStop | Either / respective audio channel | C | Direction/epoch and reason; immediately fences queued old samples; cannot enable the other direction |
| 0x0070 | FileOffer | H↔K / files | F | Transfer ID, approved source/destination handle, bounded relative name/metadata, size and ATP profile; A and current controlling session, local directory/selection policy |
| 0x0071 | FileAccept | Receiver→sender / files | F | Offered transfer, ATP capability/profile and admitted byte/rate/concurrency budgets; valid offer and authorized endpoint |
| 0x0072 | FileChunk | Sender→receiver / files | F | Transfer ID and bounded payload of the pinned ATP profile; accepted transfer, current authority and credit; no control-parser exception |
| 0x0073 | FileComplete | Receiver→sender / files | F | Transfer ID, verified ATP completion and publication stage; integrity verified and atomic publication completed, or explicit partial/unknown result |
| 0x0074 | FileCancel | H↔V / files | F | Existing transfer and reason; permitted in Z; stop admission and preserve explicit committed effects |
| 0x0075 | SyncJobState | H↔K / files | F | Locally configured job ID, direction, keep-both conflict policy, ATP progress/status; current controlling session; client acceptance cannot create arbitrary host jobs/paths |

Clipboard echoes are suppressed by source/sequence; read-only viewers never
receive clipboard or file access. Whole clipboard item validation precedes OS
publication; setting a clipboard is not application paste. Image clipboard is
an optional bounded ATP transfer, never a text-control payload.

Each audio direction has its own enable, generation, and budget. Downlink may
serve an admitted read-only viewer when locally authorized. Uplink requires an
explicitly enabled controlling session and a selectable qualified virtual mic;
connecting never activates it. Device change, disconnect, background, or
revocation invalidates old samples; silence/gaps are explicit. Endpoint-wide
playback capture scope is disclosed and locally approved. Remote messages
cannot enable host audio globally or install a virtual device/driver.

File messages are an authorization/stream envelope for **existing ATP**;
they do not define a competing chunk-repair, hashing, resumption, or sync
algorithm. The exact upstream ATP revision/profile and codecs must be selected
and tested by the transfer slice before advertising this capability. A resume
requires fresh session authority and a newly admitted channel; it never restores
an input lease. Only host-selected/configured directories and client-accepted
jobs are addressable. Validate traversal and symlink escape before writing;
temporary partials publish atomically, conflicts keep both, and nothing is
automatically opened/executed. Clipboard/text/file/audio payloads never enter
logs or error strings.

### Feedback

| Kind | Message | Sender / channel | Bound | Required content and state constraints |
|---|---|---|---|---|
| 0x0080 | PresentedState | V→H / control | C | Current binding/frame, source-observation identity, decode/presentation stage, clock/uncertainty, visible/stale/unknown state; A; only valid current evidence may establish R |
| 0x0081 | ReceiverPressure | Receiver→sender / control | C | Direction/binding, credit epoch, cumulative freed bytes/frames and queue pressure; admitted channel; §5 credit rules, no authority renewal |
| 0x0082 | StageMetrics | Either / control | C | Bounded sanitized stage/count/byte/duration samples with clock/scope; admitted session; no raw library strings or content |
| 0x0083 | QualityDecision | H→V / control | C | Current binding, selected operating point and bounded reason; A; inside admitted capabilities, explicit reconfiguration for codec/geometry change |

## 7. Media completeness, recovery, and freshness

Baseline video is HEVC Main, 8-bit, 4:2:0. Configuration includes exact `hvcC`;
each admitted access unit consists of four-byte-length-prefixed NAL units.
Native media adapters may normalize to Annex B. Parameter sets and declared
`hvc1`/`hev1` codec identifier must agree with the actual access units; validate
VPS/SPS/PPS, NAL count, dimensions, bit depth, layers, crop, reorder/DPB demands,
and all allocation sizes before decoder configuration. This follows the plan's
chosen form using the [HEVC WebCodecs registration](https://www.w3.org/TR/webcodecs-hevc-codec-registration/).
An unannounced parameter change refuses the AU and requires explicit recovery/
configuration, not optimistic decoding. No other video codec is negotiated.

Fragment fields use `u64` frame IDs and timestamps and `u32` total bytes,
offset, index, count, and nonzero fragment stride. Packetization is canonical:
`count = ceil(total / stride)`, `offset = index * stride`, `index < count`,
and payload bytes equal `min(stride, total - offset)`. All arithmetic is checked
before allocation, including the ceiling division; `total > 0` and `total <= A`.
Stride is fixed for an AU and fits that path's record budget after all headers.
Only the last fragment can be short. Header identity/metadata is immutable for
an AU. The baseline dependency is either independent IDR or the previous
reference frame in that binding; richer reference structures need a separately
qualified capability. Identical duplicate fragments do not allocate again;
conflicting duplicates, overlaps, inconsistent totals/counts/dependencies,
zero-length fragments, and out-of-range offsets reject the affected AU.

Reserve payload and fragment bookkeeping against W and B before admission;
a large count of tiny fragments is not free metadata. Missing ranges in
`RepairRequest` are sorted, disjoint half-open fragment-index intervals within
the announced count. Clamp their count, retry/rate/bytes, and reference horizon
(starting ceiling 250 ms). Repairs count against the same congestion budget as
new media. Sender cache has both byte and time bounds. No incomplete picture
or known-broken dependency reaches a decoder.

Display deadline and reference-usefulness deadline are separate. A late
reference may be repaired only while it can unlock useful dependent pictures
inside the admitted horizon; it need not itself be presented. Beyond the
budget/horizon, fence that subscription's chain and request a smaller fresh
recovery point. Do not damage a healthy viewer's chain or free driver-owned
surfaces to satisfy a count. Announce frames/progress on a bounded reliable
path even before idle, so a missing final fragment or entirely lost final
frame is detected without requiring a later video datagram. Failure to receive
bounded source progress makes freshness unknown, not implicitly unchanged.

Startup/reconfiguration is strictly non-circular:

```text
DecoderConfiguration -> DecoderConfigured (API configured)
    -> RecoveryAccessUnit (complete verified IDR)
    -> FirstFrameDecoded -> PresentedState (current usable view)
```

Each milestone has an admitted timeout and cleanup path. Only one recovery AU
per subscription is outstanding, on its dedicated bounded reliable channel;
shared pipelines coalesce requests into at most one IDR-production attempt at
a time and separately bound delivery retention for all subscribers. IDR chunks
use the same checked total/offset completeness rule
with reliable ordered contiguous delivery. Partial/reset recovery never feeds
a decoder. A failed attempt is fenced, bounded sends drained/abandoned, and a
fresh recovery generation admitted before another attempt. Dependent pictures
may arrive after M but wait within W/B until the IDR has decoded. A worker may
produce a bounded bootstrap IDR to obtain configuration before M; it may not
send it to an unconfigured/unapproved viewer.

Pixel-update age and trustworthy-source-observation age are separate fields.
`MediaProgress` identifies a serviced capture, an OS-qualified unchanged
observation, or unknown; an alive socket/heartbeat is not capture evidence.
`PresentedState` distinguishes decoded, compositor-submitted, visible, and
instrumented observations and states uncertainty. Old pixels do not become
fresh just because they arrived. Sustained unknown/stale presentation revokes
or suspends input before another action is submitted. Decode success alone
cannot establish visible readiness; hidden/background clients release control.
No per-frame flush or per-frame round trip is required for steady-state video.

## 8. Refusals, results, cancellation, and freeze gate

Refusal reasons are stable typed categories, not unbounded prose:
`unsupported_version`, `required_capability_missing`,
`tailnet_membership_unverifiable`, `tailnet_policy_blocked`, `origin_rejected`,
`attachment_invalid`, `approval_required`, `permission_missing`,
`invalid_limits`, `resource_limit`, `malformed_record`, `role_violation`,
`invalid_state`, `stale_generation`, `sequence_gap`, `ticket_expired`,
`lease_expired`, `view_unready`, `unknown_history_no_replay`,
`no_supported_hevc_decoder`, `recovery_budget_exhausted`, and
`microphone_endpoint_unsupported`. Unknown required reason encodings refuse
parsing; numeric assignments land with the implementing codec's fixtures.
Refusals contain bounded identifiers/codes only, never input, clipboard,
filenames, screen content, credentials, or foreign-library error strings.

Malformed framing, role violations, and security failures stop the offending
channel/session; do not scan forward to a guessed next magic value. Dropped
stale datagrams require no response allocation. A policy/capability/resource
refusal may leave permitted status/viewing work active only when its existing
authority remains valid. A rejected action is never transparently retried with
a fresh ticket. Rate-limit refusals themselves; close silently when no bounded
authenticated reply can be sent. Expiry/local revoke remains fairly serviced
during all floods.

`InputResult`/closure outcomes distinguish success, partial submission,
cancelled-before-submission, refused/expired, and unknown external effect.
Stages are **admitted**, **submitted to the OS**, and **observed**; none means
exactly-once execution. Already-submitted text is not discarded from a result,
and modifier releases are cleanup, not rollback. Request IDs/receipts are
bounded in the live epoch; there is no durable keystroke journal.

Teardown order is fixed: revoke input authority → release remotely held
keys/buttons → invalidate generations → stop capture admission → cancel
cooperative tasks → drain bounded sends → terminate a stuck foreign worker
if necessary → publish closure. Fence before cleanup, without waiting for a
remote round trip or media callback. If the peer disappears, its receipt of
`Closed` is unknown; the host still performs local teardown. Removing one viewer
only releases that viewer's shared-pipeline subscription.

Pending operations, attachments, approvals, reassembly, recovery, and closing
channels have count/byte/time bounds. Dropping a future or WebSocket close is
not evidence that queued bytes or OS effects were withdrawn. WSS replacement
fences old bindings immediately, stops writes, bounds concurrent closing
channels, and rejects late old data. Reconnect, suspend/resume, host identity
change, OS user switch/lock, worker crash, and stale-view recovery revalidate
their applicable generations/authority; no input lease resurrects implicitly.

Before v1 freezes, the wire, authority, and transport beads must provide
golden byte messages and independent-peer interoperability for **each** native
QUIC, actual WebTransport/H3, and WSS profile. Required cases include positive
negotiation, forbidden sender/state, over-limit/overflow/nesting, unknown
required/optional fields, forged/reused tickets, old bindings, duplicate and
conflicting fragments, final-frame loss before idle, distinct recovery/display
deadlines, expired queued input, partial OS effects, and receiver-credit replay.
Fixtures/simulations do not stand in for live TLS, browser, codec, or hardware
evidence. These gates remain owned by the implementation/qualification beads;
this draft neither closes them nor preselects a passing outcome.
