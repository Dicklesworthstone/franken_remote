# Ticketed native media-configuration attachment

The existing running host and viewer sessions can now negotiate an auxiliary
configuration/reply pair on their **same Asupersync QUIC connection**. The pair
is not supplied as a ready-made application route: binding acknowledgement,
one-use ticket consumption and attachment acknowledgement precede its use.
This is the configuration-channel join, not a complete remote-desktop app.

## Implemented exchange

```text
host: reserve bounded native stream identities
  -> StreamBinding on the admitted control channel
viewer: validate parent/view/selection and allocate its local stream
  -> BindingAccepted on the existing control channel
host: arm the receive window only after the peer has opened its stream
  -> ChannelTicket on the existing control channel
viewer -> ChannelAttach as the first record on its new stream
host: consume the exact ticket once
  -> ChannelAttached as the first record on its new stream
both: promote the same pair to DecoderConfiguration / DecoderReplies
```

`HostSession::offer_media_channel` takes its parent, selection, context and live
observation check from the actual admitted owner. The locally selected view and
unpredictable ticket still come from the qualified host, not a remote request.
`ViewerSession::accept_media_channel` uses that viewer's existing connection,
selection and responsiveness lifetime. Neither method admits a new peer, chooses
a desktop, grants control, enables a decoder, or disables local approval.

The non-cloneable `MediaChannel` in `fr-transport` owns one fixed 186-byte pending
record. The underlying `QuicRecords` keeps its existing byte/count-bounded send
ownership. Retrying backpressure preserves the original record and absolute
local deadline. Host expiry is echoed unchanged; the viewer never translates
host monotonic time into its own authority. The host checks the complete grant,
including parent/display generations, stream IDs, ticket, credit and expiry.

`transmit`, `dispatch` and `finish` operate on the original connection and require
the containing session's live authorization guard. Continue the normal session
driver between them, including while waiting for attachment: observation renewal
and admission refresh must not stop. Attachment dispatch leaves unrelated control
records with their original dispatcher. Application dispatch returns `Blocked`
for attachment records awaiting this owner, rather than consuming or copying an
unbounded history. No native capture or decoder call belongs in these callbacks.

The native stream allocator chooses the pair; fixed stream numbers are not a
wire-level assignment to roles. The server delays MAX_STREAM_DATA for the new
client-initiated stream until BindingAccepted: advertising it before the client
opens the stream is an invalid ordering on the pinned transport. The server's
unarmed reservation is skipped by ordinary receive dispatch without inventing
native readiness. Once armed, both directions retain normal framing and expiry.

Only the exact attachment message family is routable before promotion. After
promotion, old attachment routes no longer match; unrelated or replayed messages
cannot masquerade as decoder replies. A stream's retained retransmission bytes
keep their original charges and deadlines during promotion. Completion is not a
reset, new socket, decoder configuration, or proof of visible pixels.

## Bounds, failure and scope

Native reliable routes have a fixed 16-stream ceiling (previously eight), with
at most seven auxiliary pairs alongside the two control streams. Static routes
also count against this same ceiling. At most one attachment can be pending.
Retired binding and host ticket identities remain reserved until connection
closure; a second use refuses. Exhaustion refuses rather than recycling an ID.
Connection and per-stream native flow-control ceilings are unchanged.

Attachment lasts at most two seconds. The host's original deadline is never
renewed by approval, a packet, or a retransmission. Closing or dropping an
unfinished owner marks its reservation terminal; the next checked connection
operation, including idle service, closes the affected connection. The containing
session still owns prompt input revocation, cleanup and lifecycle scheduling.
Foreign connections, wrong parents/roles, malformed grants, permission loss and
expired reservations refuse before additional application data is admitted.

The runtime implements **MediaRole::Configuration only**. Other syntactically
valid wire roles explicitly refuse. Recovery, progress/repair, video datagram,
input and other channels retain their existing local installation requirements.
There is no wildcard role, automatic fallback, or alternate listener/runtime.
The attached configuration pair is bounded to the smaller of negotiated control
C, native receive-window capacity and critical sender capacity. Picture allowance
is zero: compressed-frame and decoded-surface budgets remain separate.

## Executed tests

The nine transport attachment tests use actual localhost UDP/TLS and the existing
Asupersync endpoint. They cover successful exchange and in-place promotion,
original deadlines, owner destruction, one-pending and seven-pair bounds,
capability/parent rejection, duplicate ticket/binding refusal, a forged ticket on
the actual stream, pre-attachment application refusal, invalid old routes after
promotion, and genuine critical-queue backpressure with unrelated renewal traffic.

A session integration runs the production host/viewer negotiation and persistent
drivers, negotiates the capability, attaches the pair, and checks that observation
renewal continues. Its admission metadata and display identity are explicit
private fixtures, not live Tailscale credentials or a new public fixture mode.

All nine existing native decoder-startup tests now use the ticket-negotiated
configuration pair. They retain actual X11 capture, supervised software HEVC,
network configuration, first-frame readback and a dependent P picture after
handoff. Their other media routes remain explicit fixtures. Existing malformed
configuration, cancellation, expiry and backpressure assertions are unchanged.

Reproduce on the pinned compiler with native SDKs installed:

```sh
cargo test -p fr-transport --test media_attachment --locked
cargo test -p frd session_startup --locked
cargo test -p fr-native --features linux-media --test decoder_startup --locked
./scripts/verify.sh fast
./scripts/verify.sh docs
```

Local runtime tests rebuild first-party Rust and native bridge code against the
exact retained Asupersync TLS libraries. A cold local dependency build exceeded
this container's memory; full fresh Cargo verification is a separate CI gate.
This evidence does not certify live tailnet sharing/ingress, independent QUIC
interoperability, hardware acceleration, WAN performance, or optical latency.
No Beads task or full application gate is closed by this configuration slice.

Related: [PROTOCOL_ATTACHMENT.md](PROTOCOL_ATTACHMENT.md),
[SESSION_DRIVERS.md](SESSION_DRIVERS.md), [DECODER_STARTUP.md](DECODER_STARTUP.md),
[QUIC_RECORDS.md](QUIC_RECORDS.md), [PROTOCOL.md](PROTOCOL.md).
