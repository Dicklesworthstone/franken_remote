# Owned host opening and responsive admission refresh

`Host::open` joins the already authenticated, admitted native connection to the
existing `HostSession`. It is the preceding application stage for
[`HostSession::publish_display`](HOST_PUBLICATION.md), not a listener or a new
identity/consent mechanism. `Host::from_admitted` still requires actual installed
Tailscale admission for the connection's exact endpoints.

## Local consent and ownership

The method consumes `Host` and accepts a bounded network-turn duration and a
nonblocking callback receiving the existing `Approval` capability and requested
protocol role. The callback is invoked at most once. It may hand the capability
to a bounded local UI queue; returning `Ok(())` only acknowledges notification.
It does not approve observation. Only an explicit, still-valid `Approval::decide`
can make that decision, and it cannot apply to another equal-ID connection.

A local notification failure ends the attempt. Silence remains unapproved until
the original startup deadline expires. Notification is skipped when local policy
already specifies that no approval is required. This method does not change that
policy, grant input, select a display, or construct visible-frame evidence.

The final `BindingAccepted` still has to arrive on the original authenticated
stream. Only then does `open` move that same admission, authority and transport
through `finish` and `into_running`. The returned owner can proceed to display
publication or the existing separately approved control acquisition path.

There is no new opening timeout. Parked time, notification, approval, peer
refresh and the final acknowledgement consume the original `Host` deadline.
The network-turn duration must be between one and one hundred milliseconds;
it is a polling bound, not a new authorization lifetime. A callback that blocks
past the deadline cannot release a late session acknowledgement.

Dropping the attempt, even without polling it, closes the host before dropping
notification callback state. The borrowed `Host::drive` now also creates its
destruction guard at method invocation, rather than inside its first poll.
Cancellation remains terminal; reconnecting requires a new admitted owner.

## Initial startup no longer stops UDP for a metadata lookup

Previously the initial host driver awaited `Admission::refresh` before servicing
its native connection. The running-session driver already avoided that problem,
but initial negotiation and the approval wait did not.

The startup driver now polls the real refresh concurrently with its existing
`QuicRecords::drive`. Queued handshake replies and transport acknowledgements
continue through that original owner. Incoming records remain bounded by the
existing transport; this does not dispatch pixels or input before consent or
create observation authority simply to keep networking alive.

The refresh monitor retains a handle to the original shared admission `Lease`,
plus the original role, local decision, startup deadline and old proof expiry.
It does not clone or reconstruct permission state. Checks run around lookup
polls and native I/O polls, including the transport's final authorization check.
Successful metadata completion must occur before the old proof's exclusive
expiry. Updated metadata cannot resurrect an expired attempt.

When refresh succeeds, an already-pending healthy network turn finishes before
the helper returns. Dropping that turn early would cancel the connection. Denial,
parent cancellation, lookup failure or expiry instead revoke admission and
retire consent before destroying pending lookup/I/O work. One fixed error slot
preserves the first cause: expiry found inside transport authorization must not
be relabelled as the consequent retired-consent state.

No lock is held during network or LocalAPI I/O. No new worker, input mailbox,
codec process, transport, runtime, proof store or retry queue is introduced.
A zero-wait borrowed drive cooperatively waits at least one millisecond during
refresh, still capped by the original proof and startup deadlines.

## Verification boundaries

Twelve new tests exercise public opening and the private production refresh
join over actual localhost UDP/TLS. Admission and controlled lookup timing are
explicit synthetic fixtures, not live Tailscale qualification. They cover
queued capability delivery during a slow lookup, consent separation, retained
connection identity, healthy in-flight I/O, original deadlines, late success,
denial, cancellation, unpolled abandonment and teardown order. The complete
21-test startup selection passed at one, four and eight test threads.

The existing real native publication helper now uses `Host::open`, rather than
hand-driving negotiation. It retains the explicit local approval fixture and all
existing assertions for discovered display selection, actual FFmpeg HEVC, X11
pixel readback, idle behavior, renewal and original worker identities. All
thirteen explicitly enabled native streaming cases passed. These tests establish
software HEVC and private X11 behavior, not physical scanout or hardware decode.

The final local runtime selection passed 122 ordinary daemon tests and thirteen
explicit native streaming tests. The ordinary run lists sixteen opt-in cases as
ignored; the separate native run executes thirteen of them. The three existing
network-namespace cases were not executed in this continuation. Runtime sources
were freshly rebuilt with nightly-2026-08-31 against matching retained dependency
artifacts; this is not a cold full-workspace dependency rebuild. Shared crates
were separately tested through their isolated Cargo workspace.

Two independently compiled negative controls failed unchanged regressions:
returning on refresh success before native I/O completes, and deferring the
failure fence until after the pending lookup is destroyed. An initial parallel
run exposed expiry being relabelled as denial; the first-error slot fixes that
race without weakening the exact error assertion or changing a deadline.

The complete recovered host publication preceding this change separately passed
clean full-workspace CI run `34710743593`. That earlier result does not cover this
new opening/refresh increment; committed-source CI must verify it independently.

## Still outside this implementation

A qualified daemon listener, OS consent interface and physical input/visibility
adapters remain separate application work. `Host::open` consumes an already
admitted native connection; it does not turn a node-address bind or an IP-prefix
check into qualified tailnet ingress.
