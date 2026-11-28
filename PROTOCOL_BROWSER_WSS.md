# Browser video over dedicated WSS (v1)

This is the positively negotiated `browser-video-wss` version **1** capability
within WSS transport profile **3**, profile version **0**, application version
**0**. It extends [browser bootstrap](PROTOCOL_BROWSER_BOOTSTRAP.md), ordinary
[negotiation](PROTOCOL_NEGOTIATION.md), [decoder startup](PROTOCOL_DECODER.md)
and [media](PROTOCOL_MEDIA.md). Native profile-1 codecs remain unchanged.
Independent interoperability and live browser qualification remain open.

## Profile and authority

The implemented role is Observe. The required capability set is
`browser-video-wss` v1, `clock-correlation` v1 and `hevc-decoder-startup` v1.
The host must enforce the selected local approval policy before SessionOpened
or capture. A host without a browser approval backend refuses local approval;
it cannot silently turn that policy into unattended observation.

There are two physical TLS/WebSocket connections. Connection control carries
negotiation, observation challenges/renewals, clock exchange and cumulative
ReceiverPressure. A separately attached video socket carries configuration,
recovery and encoded video records under one full immutable decoder binding.
It does not invent separate QUIC stream IDs. Video retains TCP head-of-line
blocking; the diagnostic profile is `wss_degraded_tcp_hol_dedicated`.

Both sockets require their actual original installed-Tailscale admissions and
the existing exact-Origin/Host/SNI checks. Auxiliary attachment must compare
the complete authenticated node/peer identity, original LocalAPI provenance,
daemon, admission profile, policy, permissions, exact local endpoint and peer
IP against the control owner. Only the remote ephemeral TCP port may differ.
Both leases must remain independently live. A match grants no new lifetime.

## Host-local approval

`browser-local-approval` version **1** is optional in the client offer. A host
whose original local policy requires consent marks it required in its offer.
Peers without that capability cannot open an approval-required desktop. An
optional capability alone does not require a prompt or enable approval on an
unattended host.

After verified node/origin/nonce admission and SelectedConfiguration, the host
sends exactly one canonical ApprovalRequired before waiting on its existing
native consent UI. Its request is the original remote-session ID; its role is
Observe; its deadline is nonzero host-monotonic time. The native prompt shows
the actual admitted device and negotiated effects. The prompt owns only a weak,
one-use decision capability. The original pending owner must consume a positive
decision after prompt cleanup before observation authority, discovery, capture,
SessionOpened or video tickets can be created.

Initial negotiation retains its original ten-second ceiling. A valid notice
received before that ceiling selects a total consent/startup ceiling of thirty
seconds from the **original** client attempt and host control start, respectively.
Receiving the notice never starts a new thirty-second timer. Nonce expiry still
bounds issuance and host consumption of the one-use ClientHello; consuming that
nonce does not turn it into a reusable credential. The client does not compare
the uncorrelated host timestamp with its local clock.

Repeated notices, changed requests, changed roles, or a SessionOpened naming a
different request are terminal. A required approval capability also requires
the original notice before SessionOpened. The client displays a waiting state
without media or input readiness. Denial and unanswered consent have distinct
refusals. Socket closure, hidden-page retirement, original admission loss, local
Stop, selected-session loss and policy change retire the pending capability;
late native answers cannot authorize a replacement. The host retains native
prompt cleanup independently of the cancelled socket.

## Initial display selection

The optional existing `display-selection` version **1** capability uses the
canonical reliable control-channel DisplayCatalog and SelectDisplay records.
After any required consent, SessionOpened and its queued BindingAccepted, the
host discovers one catalog from the original capture worker and publishes at
most eight displays. The catalog is bound to that control session and carries
opaque aliases, its original revision and each display's geometry generation.

The Rust client retains this one immutable catalog. A sole display is selected
automatically; multiple displays require an explicit initial UI choice. The
JavaScript API supplies only an index through an attempt-bound callback. Rust
resolves that row and emits the exact full-display selection; JavaScript cannot
construct a native display name, handle, revision, geometry or crop request.
The choice must actually enter the control socket before a corresponding media
binding is accepted. The binding and visible picture dimensions must match the
chosen display.

The host accepts exactly one selection from the original catalog and validates
the complete choice again through the same discovered worker before encoder
configuration. Renewal, clock, local revocation and socket closure continue
while the picker is open; topology changes are checked without capturing pixels.
The original startup ceiling still applies: ten seconds normally, or thirty
seconds total when a timely local-consent notice selected that budget. Catalog
arrival and user choice never start a new timer.

Empty catalogs, stale or foreign choices, crops, duplicate/replacement catalogs,
repeated selections and mismatched media bindings are terminal. The original
worker's cleanup remains retained if the wait is cancelled. No mid-session
switch or worker replacement is implied. Without the negotiated capability,
the host retains its existing local origin/first-display selection.

## Video attachment

After SessionOpened and its control BindingAccepted, the host sends a video
StreamBinding on the control socket. Its payload is:

| Field | Encoding |
|---|---|
| Video binding ID | u32; nonzero and distinct from the control binding |
| Host boot, OS session, remote session | Three original 16-byte IDs |
| Display | Original 16-byte display handle |
| Geometry, codec configuration, recovery, viewport | Four original u64 generations |
| Role | u8, exactly 3 (video) |
| Direction | u8, exactly 1 (host-to-viewer application media) |

The complete descriptor is 102 payload bytes. Every parent identity must equal
the original control tuple; every media generation is explicit and nonzero.
The viewer accepts the descriptor with BindingAccepted: the header retains the
control binding and the four-byte payload names the video binding.

The host then sends ChannelTicket on control. Its payload repeats the complete
descriptor followed by:

| Field | Encoding |
|---|---|
| Ticket | 32 opaque, unpredictable bytes; never a URL or diagnostic |
| Deadline | u64 original host-monotonic microseconds |
| Outstanding-byte allowance | u64 |
| Outstanding-picture allowance | u32 |
| Credit epoch | u64, nonzero |

This is 162 payload bytes. ChannelAttach is the first binary application
message on the video socket and repeats the **entire exact grant** under the
video header binding. After checking both original admissions, the host
atomically consumes its role-specific ticket and returns ChannelAttached
with the same grant. An ID alone, changed limit, changed epoch, expired grant,
replay, wrong peer, or second attachment refuses. The current owner permits
one attachment only; replacement/reconnect is a new session.

## Decoder and media ownership

The attached video socket uses the canonical DecoderConfiguration,
DecoderConfigured, RecoveryAccessUnit and FirstFrameDecoded sequence. Ordinary
FrameProgress and encoded payload/recovery records retain their existing
schemas, source evidence, reference relationships and decoder binding. Each
binary WebSocket message contains exactly one bounded FRD0 record. Dispatch
validates the complete envelope, kind and logical channel before the specific
canonical decoder checks the original binding and state.

Host selection comes from its actual discovered display catalog. The real
capture worker, selected encoder and decoder configuration are retained by
the original desktop owner. Neither SessionOpened nor a configured decoder
proves a decoded or visibly presented frame. View-only readiness never creates
an input lease. Original source/clock evidence and display deadlines remain
required at output and final presentation.

## Receive credit and bounds

The video record ceiling is 65,536 bytes. Outstanding compressed-resource
credit is at most 2,097,152 bytes and four pictures, additionally bounded by
the negotiated protocol limits and the host's configured ceilings.

Before the first payload fragment, the host sends canonical FrameProgress and
charges the complete picture as `3 * encoded_access_unit_bytes + 4096`. The
maximum encoded access unit is therefore 697,685 bytes. The overlapping copies
and bookkeeping reserve account for receive, WASM conversion and WebCodecs input ownership;
a host whose selected whole-picture bound cannot fit its granted credit must
refuse before starting capture. A partial write does not refund the charge.

The receiver retains that original reservation through reassembly, queued
decode, original callbacks and escaped native frame ownership. Queue pruning
and resource closure reconcile the same reservation even when no later packet
arrives. Cumulative ReceiverPressure returns bytes and picture counts only
after those resources are released, under the original channel and credit
epoch. Writability, outbound bufferedAmount, receipt, decode and presentation
are not interchangeable credit receipts.

Expired authority and obsolete generations prevent further writes, decoder
admission or freshness claims. Source retirement is retained independently of
either socket future; a closed transport is not evidence of worker exit.

