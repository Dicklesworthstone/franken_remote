# PROTOCOL.md — FrankenRemote Wire Specification

> **Status: reserved, not yet specified.** This file will become the one concise, normative wire specification for FrankenRemote. Until it carries versioned normative content, the authoritative source for protocol shape, message classes, limits, and compatibility rules is **§17 of [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md`](COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md)**, together with the transport and recovery semantics in §12 and the input semantics in §15. Nothing in this stub is an implemented format.

## What this document must contain before version 1 freezes

Per plan §17.1, one concise specification covering:

- version negotiation, limits, identity binding, capability selection;
- session generations and compact stream bindings (session, display, codec configuration, recovery chain, viewport);
- media configuration and the non-circular startup/recovery handshake;
- input semantics: ordered action sequences, replaceable pointer states, tickets, expiry, and partial-effect reporting;
- error taxonomy and teardown;
- explicit byte order, field widths/varint encoding, timestamp units, signed-coordinate representation, maximum nesting, and canonical error handling;
- allowed sender role and state per message (a malicious host cannot send a host-input command a client applies to its own OS);
- the negotiated limits table (starting points in plan §17.2), kept in one tested structure.

## Freeze requirements

- **Golden messages and an interoperable test peer ship before version 1 freezes**, covering every transport profile (native QUIC, WebTransport/H3, WSS) — not only a self-round-tripping encoder/parser pair.
- High-frequency traffic uses a bounded binary framing (fixed header plus bounded length-delimited fields); no extensible serialization ecosystem, and network input is never deserialized into an unconstrained object graph.
- The protocol version, robot-schema version, capability names, transport profile, and WebTransport draft compatibility are versioned separately. Required unknown features cause a typed refusal; optional unknown fields are ignorable only under a bounded framing rule.
- A WSS fallback retains the same authorization, message bounds, and input-expiry semantics; downgrades that remove required security checks are rejected.

## Proposed message classes (from plan §17.1 — names are proposals, not implemented APIs)

| Class | Representative messages |
|---|---|
| Negotiation | ClientHello, HostCapabilities, SelectedConfiguration, Refused |
| Session | SessionOpened, ApprovalRequired, LeaseGranted, LeaseRevoked, Challenge, ChallengeResponse, InputTicket, ChannelAttach, Closed |
| Displays | DisplayCatalog, GeometryChanged, SelectDisplay |
| Media | DecoderConfiguration, DecoderConfigured, RecoveryAccessUnit, FirstFrameDecoded, AccessUnitFragment, RepairRequest, RecoveryRequest |
| Input | KeyTransition, ButtonTransition, PointerState, RelativeCheckpoint, Scroll, CommitText, HeldState |
| Auxiliary | CursorShape, ClipboardBegin/Chunk/Commit, AudioConfiguration, AudioPacket (audio messages carry an explicit direction: playback-down and microphone-up are independent negotiated capabilities per plan v1.4) |
| Files | FileOffer, FileAccept, FileChunk, FileComplete, FileCancel, SyncJobState (the plan §15.6 transfer/synchronization envelope on its own bounded channel) |
| Feedback | PresentedState, ReceiverPressure, StageMetrics, QualityDecision |
