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

