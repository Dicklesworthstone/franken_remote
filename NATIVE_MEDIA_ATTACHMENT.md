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
configuration, cancellation and expiry refusals remain required. Two test-setup
assumptions were corrected for the genuine attachment ACK still occupying QUIC: a
foreign-connection refusal preserves the entire prior queue accounting, and the
pressure fixture waits for real admission before testing backpressure. Neither
change resets a deadline, changes production code or substitutes timeout for a
specific rejection.

## Published source and retained verification

The bounded wire records were published in `24788e1eff192a75e5402d4f2ca742912b7256ec`.
The runtime, session integration and native test migration were published in
`1c733934bed6cabdb8f793e206f075af4b78ba4a`, preserving concurrent control-renewal
work through `184e0092eaf713aab84d298dbcd5cfc760b098f5`.

[Run 34474629312](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34474629312)
verified the exact five wire source objects. The corrected runtime's exact ten
source objects passed full pinned-toolchain formatting, workspace compilation,
strict Clippy, tests and documentation checks in
[run 34478447265](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34478447265),
including explicit runs of all nine live attachment tests, all 26 session-startup
tests and all nine native decoder-startup tests. The workflow staged only the
hash-checked reviewed patch, then exported the matching source objects after
success. Its base was `8d0befc`, before concurrent client/control-renewal publication;
that evidence does not automatically cover later combined checkouts. The retained
first runtime candidate run `34477797403` failed on the queue-empty test assumption
above; it is not counted as passing evidence.

Locally, the final nine native tests passed ten four-thread repetitions. The nine
attachment, 26 session and nine native tests also passed three combined parallel
runs. Sixteen existing live-QUIC regressions passed, as did the earlier 320-test
core/wire/media/client Cargo selection, which predates the concurrent client
changes. Selected first-party and test Clippy checks passed. These are separate
selections and repetitions, not an invented full-suite count.

A negative control removed only ticket-identity comparison from a separate source
copy. The unchanged forged-ticket test then failed; the production implementation
passed. Deadline, tuple, transport and application-refusal assertions were retained.

The finalized workflow checks committed source read-only, without candidate
patches, source-object writes, elevated token permissions or branch mutations.
UBS and Beads tooling were unavailable; no issue, task or phase was closed.

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
