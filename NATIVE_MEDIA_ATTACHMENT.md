# Ticketed native media attachment and delivery

The running host and viewer sessions negotiate configuration, recovery, and
video/progress/repair channels on their **same Asupersync QUIC connection**. The
native HEVC path now consumes those completed attachments instead of requiring
preinstalled media routes. The initial session-control pair and identity/display
fixtures remain supplied by the test harness; this is not yet a complete
Tailscale-admitted remote-desktop application.

## Implemented exchange

Each role uses the existing one-use attachment exchange:

```text
host -> StreamBinding on the admitted control channel
viewer -> BindingAccepted after opening its actual stream
host -> ChannelTicket after arming the receive window
viewer -> ChannelAttach as the first record on its auxiliary stream
host -> ChannelAttached as the first record on its auxiliary stream
both -> activate only the exact negotiated message family
```

Configuration uses the `native-media-attachment` capability. Recovery and Video
additionally require `native-media-delivery`; old configuration-only peers do
not silently gain those roles. Stream identities are allocated, not fixed wire
role numbers. `HostSession::offer_media_role` and
`ViewerSession::accept_media_channel` retain their actual connection, selection,
parent identity and observation checks. The locally selected display and
unpredictable ticket are host-owned, never selected by an untrusted request.

| Role | Host to viewer | Viewer to host | Delivery |
|---|---|---|---|
| Configuration | DecoderConfiguration | DecoderConfigured, FirstFrameDecoded | Critical reliable pair |
| Recovery | RecoveryAccessUnit chunks | No application records | Bulk reliable host stream |
| Video | Progress and video fragments | RepairRequest | Critical feedback pair plus bounded video datagrams |

The Video role uses one binding across its datagrams and progress/repair pair.
Record kind, native stream, direction and delivery class remain exact. Recovery
has a distinct binding. `MediaBindings::negotiated(video, recovery)` explicitly
represents this layout; the legacy four-ID constructor still rejects aliases.
Neither constructor treats numeric IDs as authorization.

## Joining completed channels to native media

`frd::media_quic::NegotiatedMedia` is constructed from three actual completed
`MediaChannel` owners, not copied route descriptors. It requires the original
live connection, correct roles, identical host/OS/remote-session and display,
geometry, viewport, codec and recovery generations. The protocol limits retained
by every attachment must equal the supplied session selection. A new selection
cannot widen already-negotiated resources.

The joined owner supplies decoder setup, the host sender, viewer receive
configuration, exact inbound media dispatch, and the repair stream. The shared
packetizer ceiling is the minimum of the real recovery/video/feedback allowances,
including the datagram cap. Decoder-configuration limits and compressed/decoded
picture reservations remain separately enforced.

The sender requires the approved observation owner for the matching remote
session. It retains its original connection identity after construction, through
send, drive and checked repair dispatch. Another connection with the same
numeric route IDs cannot receive the prepared media packet. Refusal closes this
sender's storage without touching the unrelated connection's queues or revoking
another observation. Viewer dispatch similarly refuses a foreign connection
before invoking an application callback.

The native path now executes:

```text
three ticket-negotiated roles
  -> source-owned bootstrap HEVC IDR
  -> network DecoderConfiguration
  -> validate and configure supervised native decoder
  -> network DecoderConfigured
  -> retained IDR through negotiated bulk recovery
  -> native decode, presentation and FirstFrameDecoded
  -> hand off the same decoder and receiver
  -> dependent video through negotiated datagrams
  -> progress and selective repair on the negotiated feedback pair
```

Configuration, successful API configuration, decoding, presentation and control
remain distinct. No media acknowledgement grants input authority. Attachment is
not permission to choose a desktop, bypass local approval, enable audio, create
an input lease or start arbitrary code. Native work stays outside synchronous
transport callbacks and the session's independent authority path.

## Bounds and failure behavior

`MediaChannel` retains one fixed 186-byte pending record. Backpressure preserves
its bytes and original absolute deadline. Attachments last at most two seconds;
packets, approvals and retries do not extend them. An abandoned incomplete owner
marks its reservation terminal; the next checked connection operation, including
idle service, closes that connection. Observation renewal and Tailscale refresh
must continue while attachment waits.

There are at most 16 reliable routes: the two control streams and at most seven
auxiliary pairs. At most one attachment is pending, and at most four video
datagram routes are installed. Static routes count against the same ceilings.
Retired binding/ticket identities remain reserved until connection closure;
exhaustion refuses rather than recycling identities.

Receive credit for a client-initiated stream is armed only after BindingAccepted.
Advertising it before the client opens the stream is invalid on the pinned
transport. Before activation, only the attachment message family is routable;
after activation, replayed attachment records no longer match. Retained native
retransmission bytes keep their original accounting and deadlines. Recovery
traffic is bulk and cannot consume the separate critical-record allowance.

The joined sender retains the existing `Egress`: one exact prepared media record,
not another queue. Closing it does not cancel a shared encoder. The enclosing
session still owns timely input revocation, held-key/button cleanup, generation
invalidation, cooperative drain and supervised worker termination. Connection
identity and media closure do not themselves prove OS cleanup or optical scanout.

## Executed evidence

The 12 native decoder-startup tests use actual localhost UDP/TLS, private Xvfb
servers, real X11 capture, direct FFmpeg software HEVC, supervised child-process
decoding/presentation and pixel readback. All three media roles attach on the
network before decoder startup. Only the initial control pair is installed by
the fixture; no configuration, recovery, feedback or video route is preinstalled.

The successful path verifies an IDR and a subsequent dependent P picture without
restarting the decoder or receiver. A second path drops every original datagram
of the final dependent picture **after genuine QUIC reception**, then requires
reliable progress and a repair request sent through the negotiated reverse stream
to restore that picture. No additional capture is taken after the loss. This is
application-boundary loss injection, not physical-link or WAN qualification.

The remaining tests preserve malformed HEVC/geometry rejection before worker
launch, failed native startup without premature IDR release, early/foreign
acknowledgement refusal, idle expiry, premature media refusal, genuine transport
backpressure, cancellation and worker reaping. Added checks reject mixed-role or
foreign completed owners, altered negotiated limits, foreign media/repair/receive
connections, and another session's otherwise-live observation authority.

The final local selection passed 362 core/wire/media/client Cargo tests, 14 live
attachment tests, 12 native decoder-startup tests, six existing native media/QUIC
tests, 16 existing live-QUIC tests, seven media-egress tests and 26 session-driver
tests: **443 selected tests, zero failures or ignored cases**. The 12 native tests
also passed eight four-thread repetitions and separate one-, two- and eight-thread
runs. Selected source/test strict Clippy, formatting and documentation checks passed. Runtime
and native local tests rebuilt first-party Rust and C sources against the exact
retained Asupersync 0.4.10 libraries; they are not a cold dependency build.

A separate negative-control source copy removed only the media sender's original
connection comparison. The unchanged foreign-connection test then failed because
a progress record was accepted by the other connection. The production source
and test assertions were untouched and passed. This demonstrates that the test
can detect the prohibited redirect, not merely successful ordinary delivery.

## Publication and CI history

Wire attachment first landed in `24788e1`, followed by the configuration runtime
in `1c73393`. Those stages retained successful exact-source verification in runs
34474629312 and 34478447265. The initial runtime candidate 34477797403 failed on
an incorrect empty-queue test assumption and is not passing evidence: the final
test correctly preserves complete queue accounting when an attachment ACK remains
retained. No rejection or deadline requirement was relaxed.

Recovery/video attachment landed in `fceb7a5`, after exact-source run 34490490417.
That committed revision passed Rust verification 34493282074 and full media-worker
verification 34493282089. The explicit shared media-binding layout landed in
`ea87043`, with its 111 local media tests including cross-channel rejection and a
41-picture reordering/duplicate/loss/repair sequence.

The joined native source landed in `9bde835eb65ba6d16d40b50397e81da7cb12e212`.
Its six exact source objects passed clean pinned-toolchain workspace formatting,
compilation, strict Clippy, tests and documentation checks in
[run 34498548083](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34498548083),
against `976594b0d05c05797c54f810bdaabb36a57243c5`. That run additionally executed
all 14 live attachment and all 12 native decoder-startup tests, then exported the
matching Git source objects only after success. The published implementation
uses those exact objects. The workflow now verifies committed source read-only,
without staged patches, source exports, elevated permissions or branch writes.

Verification of a later combined checkout is distinct from that exact-source
candidate result. Earlier configuration-only success or a queued workflow is
not counted as qualification of this increment.

Reproduce on the pinned compiler with native SDKs installed:

```sh
cargo test -p fr-media --test negotiated_delivery --locked
cargo test -p fr-transport --test media_attachment --locked
cargo test -p fr-native --features linux-media --test decoder_startup --locked -- --nocapture
cargo test -p fr-native --features linux-media --test media_quic --locked
./scripts/verify.sh fast
./scripts/verify.sh docs
```

This evidence does not certify live tailnet sharing or protected ingress,
independent-peer QUIC interoperability, hardware acceleration, WAN performance,
optical latency, browser/mobile clients, input-channel attachment, installers or
the final application loop. Asupersync QUIC remains primary. No dependency,
runtime, codec or identity-policy substitution was made. UBS and Beads tooling
were unavailable; no issue, task or full application gate was closed.

Related: [PROTOCOL_ATTACHMENT.md](PROTOCOL_ATTACHMENT.md),
[SESSION_DRIVERS.md](SESSION_DRIVERS.md), [DECODER_STARTUP.md](DECODER_STARTUP.md),
[MEDIA_QUIC.md](MEDIA_QUIC.md), [QUIC_RECORDS.md](QUIC_RECORDS.md),
[PROTOCOL.md](PROTOCOL.md).
