# Persistent native session drivers

`frd::session_startup::{Viewer, ViewerSession, HostSession}` joins the existing
startup, observation-renewal and transport owners. Asupersync QUIC remains the
primary transport. There is no new runtime, codec, identity mode or public
listener. This slice removes the need for every native application to implement
its own negotiation-to-renewal loop.

## Connection ownership and startup

The host continues to start through `Host::from_admitted`: the established native
connection must match the installed-Tailscale admission's actual endpoints.
`Host::finish` still requires the matching `BindingAccepted`, optional local
approval, and a live observation grant. `OpenedSession::into_running` transfers
that owner into `HostSession` and attaches its **one** observation renewer.
Neither transition grants input or creates a usable view.

The new `Viewer` drives the existing shared `fr-client::startup::Startup` over
the real TLS-established QUIC control streams. It validates the selected offer,
retains one exact pending record across backpressure, reports an optional local
approval notice, and installs the negotiated binding on those same streams.
The host and viewer have independent monotonic clocks; host timestamps are not
converted into client authority.

Local completion means the client's `BindingAccepted` entered transport
ownership, not that its bytes have reached the host. The viewer keeps pumping
that exact connection after local completion. `Viewer::finish` transfers the
same connection, routes and retained writes into `ViewerSession`; it does not
open a replacement socket or replay negotiation. This distinction is covered
by the live startup tests with a one-record critical send budget.

## Persistent service

Call the respective driver's `drive` during idle as well as active media:

- `HostSession::drive(wait, fresh_nonce, other)` consumes observation responses,
  issues due host challenges, services QUIC, and revalidates admission when less
  than 500 ms of its existing lifetime remains.
- `ViewerSession::drive(wait, other)` receives host observation challenges and
  sends the existing bounded responder's exact reply. A response retains its
  original one-second client queue deadline through backpressure. Missing valid
  host challenges triggers a three-second client-local responsiveness cutoff,
  not a claim to know or extend the host's authority deadline.

The maximum requested I/O wait is 100 ms. A needed host revalidation can span
several such turns, but is bounded by the old proof's original deadline.
A single `Admission::refresh` future remains owned while QUIC and synchronous
application dispatch continue. The implementation never drops a healthy QUIC
I/O future merely because refresh finished first: it completes the current
bounded I/O turn before handing the connection back. Failure, expiry or
cancellation instead terminates the operation. A ready refresh that crossed
the old deadline is refused; no delayed success resurrects it.

Remaining in the same drive during refresh does not imply concurrent mutable
access to the connection. This is one cooperative owner, not a new task per
packet. Admission I/O never holds the observation mutex. Between completed
network turns the refresh pump yields to the Asupersync scheduler.

`fresh_nonce` must be the qualified host-owned unpredictable nonce source.
Deterministic counters in tests are **not** a production nonce source. The
existing renewal implementation remains responsible for cadence, pending
challenge identity, original issue-time deadlines and replay refusal.

The `other` callback is the application's existing typed, bounded dispatcher.
Observation and control challenge scopes remain separate. Unknown, malformed
or unimplemented operations should refuse; the driver does not guess input
semantics. `Disposition::Blocked` leaves the exact record with the transport
under its existing memory and time limits. Callbacks must not block on native
capture, decoding, input submission or other external I/O. Control-stream
ordering is preserved, including its inherent head-of-line behavior; the
containing broker must use the separately specified media/input channels and
bounded handlers rather than putting bulk operations on control streams.

`io()` loans the same bound connection for the existing media, clock and input
owners. It does not install or authorize auxiliary channels. Replacing that
loaned connection is forbidden and the next owner check rejects substitution.

## Cancellation and authority boundaries

All driver contexts must belong to the dedicated remote session, not the whole
daemon. A dropped driver, failed turn, missing nonce, protocol/dispatch refusal,
revocation or peer closure ends the affected session. A drive guard is installed
at call time, so even dropping an unpolled drive is terminal. Host closure revokes
observation and admission before closing transport; escaped observation clones
cannot keep sending. The existing independent input owner/watchdog remains
responsible for release-only cleanup and honest late native receipts.

A successful handshake, renewal, `DecoderConfigured` or `FirstFrameDecoded`
still grants no keyboard/mouse authority and supplies no evidence of current
visible pixels. The existing presentation/source freshness, input lease,
validity ticket, geometry generation, admission and final native-submission
checks all remain necessary. These drivers do not introduce an alternative
unadmitted constructor or reissue uncertain external effects.

## Verification

The viewer sources published in `fecd20b9cf0e173edbce0fdf4d3fc8afe2abf7bb`
are the five exact objects that passed full pinned-toolchain workspace
formatting, compilation, strict Clippy, tests and docs in
[run 34424908108](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34424908108).
The host sources published in `93ffb6a13e8d6cf83dcd085ba04809581862fb47`
are the three exact objects separately qualified by
[run 34425613891](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34425613891).
That host candidate was verified on `99bd5bd`, before concurrent input-ticket
commit `46b7f70`, which was preserved during publication.
Each candidate workflow checks original and resulting source hashes before
exporting source blobs. Candidate evidence is not automatically evidence for
later concurrent repository revisions.

Local pinned-compiler runs passed **25 session-startup tests**, including
16 new driver tests, and three four-thread repetitions. Another Cargo selection
passed **297 core/wire/media/client tests**, and all **64 existing frd integration
tests** passed with strict first-party/test Clippy. These local selections precede
the concurrent ticket-overlap publication. Runtime-bound local checks rebuilt
first-party libraries and tests against exact retained Asupersync TLS libraries;
they are not represented as fresh dependency builds. UBS and Beads mutation
were unavailable in this execution environment; no task or phase was closed.

The tests use actual localhost UDP sockets and TLS, real production startup
and renewal codecs, and the same bound QUIC connections. They cover:

- Negotiation, optional local consent, binding acknowledgement and connection
  handoff; continued observation beyond its original three-second lifetime.
- Exact replies through real native backpressure; separate control-message
  dispatch; silent peers, nonce failure, permission loss, and dropped polled
  or unpolled driver futures.
- Application message delivery and renewal while a deliberately delayed
  refresh is pending; immediately successful refresh without cancelling QUIC;
  expiry during a ready refresh; observation challenges that cannot extend a
  lapsed peer proof.

A local negative control changed only the refresh join to await the entire
refresh before servicing QUIC. The unchanged delayed-refresh regression failed
because no application message was delivered during revalidation. The actual
concurrent join passed. A second negative control let successful refresh win
the race against a still-owned QUIC turn. The unchanged immediate-refresh test
then failed because the healthy observation was cancelled; finishing the I/O
turn passed. No assertions or authority deadlines were relaxed.

Reproduce on a provisioned Linux checkout:

```sh
cargo test -p frd session_startup --locked -- --nocapture
./scripts/verify.sh fast
./scripts/verify.sh docs
```

## Remaining application joins

These tests use private synthetic admission metadata and deliberately delayed
refresh futures, not a live tailnet. The actual production refresh calls the
installed `Admission` owner. Protected Tailscale-interface ingress, listener
certificate provisioning, qualified identity/sharing fixtures, negotiated
auxiliary-channel attachment, display/input acquisition, GUI and real OS
lifecycle wiring remain separate work. The LocalAPI client's bounded lookup
admission also requires broker-level multi-session scheduling; this suite does
not establish multi-viewer performance or fairness.

No new native capture/codec, WAN, independent-peer, GPU or optical-latency
qualification is implied here. This is executable connection-lifecycle
integration, not an installable remote-desktop claim. Relevant open work remains
`fr-p1-fr-client-bis` and `fr-p1-frd-broker-b9n`.

Related contracts: [PROTOCOL_NEGOTIATION.md](PROTOCOL_NEGOTIATION.md),
[TAILNET_ADMISSION.md](TAILNET_ADMISSION.md), [QUIC_RECORDS.md](QUIC_RECORDS.md),
[DECODER_STARTUP.md](DECODER_STARTUP.md), [PROTOCOL.md](PROTOCOL.md).
