# Installed-tailnet viewer connection

`frd::native_connection::Client` composes the canonical
`fr_tailnet::NativeClient` with `TargetOwner`, `QuicRecords`, and the existing
native `Viewer`. It implements one outbound, Linux session lifetime, not a host
listener, desktop consent interface, physical-display adapter, or complete GUI.
This advances the connection work in plan sections 5, 17, and 19 and beads
`fr-p1-fr-tailnet-602`, `fr-p1-fr-transport-pug`, and `fr-p1-fr-client-bis`.

## Connection and application ownership

Construct `Client::new` with the installed `LocalApi`, locally provisioned CA
roots, and a bounded TLS startup timeout. Call `Client::run` with a **dedicated
session Cx**, a `PeerSelector`, `Configuration`, the actual local capability
`Offer`, and an application closure accepting the existing `Viewer`.

The connector validates bounds before discovery, resolves exact node-owned
addresses, and delegates TLS/QUIC to the already-published native dialer. The
configured address family is explicit; unavailable families refuse instead of
racing addresses, performing DNS fallback, or changing the destination.
`Configuration::default()` uses the provisional configurable port 8443, IPv4,
and a thirty-second application startup deadline. The native profile's 64 KiB
stream and 512 KiB connection windows must match the record policy before dialing.
This is not permission to enlarge a negotiated application record or media budget.

After actual TLS and the dialer's post-handshake metadata revalidation,
`ConnectedPeer::into_owned_connection` moves the original connection **and** its
original authenticated target into a renewable owner. A parked, expired result
cannot use that transfer to acquire a new deadline. No duplicate TLS builder,
endpoint, handshake, identity lookup format, or runtime is introduced.

The application receives `Viewer` before protocol negotiation/approval completes.
Its existing startup, `finish`, running-session, controlled-viewer, and streaming
APIs remain the application path. The connector does not assume consent from
TLS, grant input, configure a decoder, or manufacture visibility. The host still
performs its independent admission and local consent checks.

`TargetOwner::serve` runs beside the **entire application future**, including
startup, approval waits, media setup and ordinary service. Successful refreshes
do not restart or cancel that future. Only the same stable node, origin and
identity projection can replace its still-live target snapshot. A late refresh
cannot revive expiry, and no lock spans LocalAPI I/O.

The connector takes an exclusive mutable client borrow for this run. It creates
no detached task, automatic reconnect, action retry, alternate transport, or
unbounded queue. The application closure and the lifetime callback must not do
unbounded synchronous work on the runtime thread.

## A retained check, not caller convention

Before exposing `Viewer`, the connector installs a connection-wide check of the
original `TargetLease`. `QuicRecords::retain_lifetime_check` allows one such
installation before application traffic. Later callers cannot replace it with
an always-true closure. It survives ordinary channel binding and all session
handoffs because those move the same connection owner.

The transport checks this lifetime at reliable/datagram admission, receive
dispatch, and before **each poll** of native flush and receive futures. A pending
native operation cannot resume solely because its original caller once passed
an authorization check. The parent cancellation and original retained reliable
record deadline are also rechecked; metadata refresh cannot extend that deadline.
Native I/O still uses bounded existing Asupersync batches. This is not a claim
of a new per-packet intercept inside an already-running native poll.

Identity failure, local cancellation, application completion, and abandonment
cancel the dedicated session Cx before dropping application work. Even dropping
an unpolled `run` is terminal. A value returned from the application cannot keep
this connection usable beyond the scope: its target owner is revoked. Native
input cleanup and codec reaping still belong to their original owners; closing
the connection does not roll back an already-submitted external effect.

## Application stream credit correction

The new end-to-end test found that the dialer's TLS handshake succeeded while
its first application stream failed with zero flow credit. The pinned native
adapter retains one conservative stream window, taking the minimum of the three
transport-parameter byte limits. Omitting unused bidirectional byte limits made
that minimum zero. The dialer now advertises the same bounded byte limit for all
three classes, while **bidirectional stream counts remain zero**. Neither the
connection byte ceiling nor stream count admission was broadened. No Asupersync
source or release pin was changed. An ordinary deterministic regression checks
these actual encoded transport parameters.

## Verification scope

Six target-owner tests use actual credential-checked Unix HTTP and the runtime:
same-lifetime renewal, old proof expiry, identity reassignment, owner/unpolled
future drop, a registered stop wake, and service past the initial deadline.

Four transport tests use actual UDP/TLS: immutable installation, reliable and
unreliable admission, receive dispatch, and revocation while native I/O is truly
pending. Removing the per-poll check in an independent source copy fails the
unchanged pending-I/O regression.

Seven ordinary connector tests exercise actual TLS and approval-gated viewer
startup with private host fixtures, configuration and trust refusal, exact
family selection, and cancellation-before-application-destruction ordering.
Three explicit tests call the **public connector** in a new network namespace,
using real Unix HTTP, exact non-loopback socket addresses, TLS and the production
startup/renewal loops. They cover sustained observation beyond initial target
expiry, changed identity while the application is pending, and expired parked
handoff. Identity metadata, CA and host admission are synthetic fixtures. These
are not live-tailnet ingress, native screen capture, or physical visibility tests.

Run the explicit session lane with `bash scripts/verify-native-session.sh`.
It builds the committed daemon tests, creates a separate user/network namespace,
checks that its network namespace differs before adding fixture addresses, and
runs all three cases. The existing `scripts/verify-native-dial.sh` separately
covers five native IPv4/IPv6 dial cases. Namespace or SDK/toolchain failures are
qualification failures; neither script changes the outer host's network.

Remaining application work includes the bounded host listener and qualified TUN
ingress, platform consent and display/lifecycle callbacks, locally provisioned
trust-store selection, and the desktop UI/bootstrap. The new connector does not
claim those broader beads are complete.
