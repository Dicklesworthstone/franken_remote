# Persistent controlled host session

`HostSession::into_controlled` consumes the existing running host and initialized
`QuicInput` into one `ControlledHost`. This joins observation renewal, control
renewal, native results, held-state reconciliation and input-ticket issuance on
the original connection. It replaces the requirement for application code to
interleave all those low-level service methods itself, including during a slow
installed-Tailscale admission refresh.

## Entry and ownership

Construct the original `HostSession` through negotiated startup, and obtain its
`QuicInput` from the initialized [broker grant](BROKER_CONTROL_GRANT.md) or the
[negotiated input join](NATIVE_INPUT_ATTACHMENT.md). Then move both into
`HostSession::into_controlled`. The attachment verifies RequestControl intent,
exact protocol limits, a ticket-capable InputFeedback route, the original QUIC
connection, and the native owner's actual shared observation-authority identity.
Equal numeric session identifiers are not sufficient. Taking the existing
one-use control-renewal capability prevents a second renewal coordinator.

The same native `Driver` must remain independently polled in its authority
region, and the OS share-session retains its original single `Seat`. This owner
creates no native thread, executor, timer, input grant, consent decision or
presentation evidence. Media workers and platform calls remain outside its
callbacks and outside authority locks.

## Drive contract

Call `ControlledHost::drive` on idle turns as well as during input/media traffic.
A turn is bounded to at most 100 milliseconds, subject to the existing required
admission-refresh behavior. Supply separate qualified host sources for fresh
challenge nonces and input-ticket identifiers. They must be unpredictable and
non-reusing; test counters are not a shipping credential source.

The owner services control responses/challenges, collects and sends native
receipts, admits ordered input before pointer traffic, and issues due tickets.
A completed input turn gives a due ticket one mailbox opportunity before the
next buffered action. The previous action's receipt still has priority: it must
be collected and accepted by transport first. This uses one scheduling bit,
not another command queue, and does not recreate or re-encode input actions.

There remains only one outstanding native command or one fixed-size pending
input-feedback record. Existing observation/control owners each retain their
one bounded challenge. Backpressure preserves the exact queued bytes and their
original exclusive deadlines. A nonce/ticket source is not called when the
corresponding owner has no issuance opportunity.

Unrelated records are delegated to the supplied bounded, nonblocking application
handler. Returning `Disposition::Blocked` retains the original transport-owned
record. Input and renewal records never escape to an arbitrary application
handler. `io` loans the same checked connection to existing media and clock
owners; it is not permission to replace the connection or install unapproved
routes.

## Admission refresh and final network authorization

The persistent host's existing refresh loop now accepts private service hooks.
While LocalAPI is pending, the same hooks keep native receipts, control renewal
and ticket issuance progressing alongside observation and UDP/TLS. The old
admission proof still expires at its original deadline; neither servicing input
nor receiving a LocalAPI response after expiry extends it.

Shared UDP submission also rechecks the native control capability, its retained
admission gate and parent cancellation. Observation authorization alone is not
sufficient for a connection carrying input tickets. In particular, a local
revoke occurring after records were enqueued but while the admission future is
polled is checked before the pending UDP operation can transmit them. The
ordinary observation-only HostSession retains its existing authorization path.

## Closure and actual native outcomes

`close`, failed service and dropping even an unpolled drive future fence input
before ending this viewer's observation and connection. Ticket expiry alone is
not control loss; ordinary valid renewal continues without extending an expired
ticket. Terminal control failure closes this controlled session rather than
silently reconstructing a read-only or newly controlled connection.

The original native watchdog and cleanup remain independently scheduled.
Neither connection closure nor cancellation claims to undo an OS operation that
was already entered. The native Seat stays occupied until cleanup and native
destruction actually finish. Other viewers' share-session capture is not owned
or cancelled by this per-viewer coordinator.

`last_reply` and `last_reconciliation` retain the canonical owner's actual
results. After closure, `collect_after_close` can retrieve an entered operation's
late result without reopening transport or retrying the action. `None` means no
new input receipt is available, not a fabricated zero-effect result; unavailable
outcomes remain typed errors. Reconciliation results remain separately readable.

## Verification scope

Eight new tests use production native startup and persistent host/viewer drivers,
actual localhost UDP/TLS, and ticket-negotiated configuration/input channels.
They exercise service beyond the original three-second authorization, native
results and tickets during a pending admission refresh, due-ticket fairness
against a genuinely buffered ordered action, unpolled cancellation, failed
credential sources, invalid waits, and revoke between refresh polling and UDP
submission. A deliberately blocked native operation crosses initial lease expiry,
returns its actual result after closure, and is cleaned up before Seat handoff.

Initial consent/control grant, display readiness, viewer visibility and the
common-runtime clock correlation are explicit fixtures. The native executor uses
a counted test sink; these new tests do not claim real X11 input effects, a hung
physical OS, hardware video, a live LocalAPI refresh or live tailnet ingress.
The test-only refresh future verifies scheduling and old-proof deadlines.

All eight tests pass with one, four and eight test threads. Removing refresh
maintenance, ticket-turn fairness, or final native UDP authorization in three
separate copies makes the corresponding unchanged regression fail. No assertion,
production timeout, input policy or credential lifetime is relaxed.

The broader local selection passes 501 tests: 372 core/wire/media/client tests
through an isolated Cargo workspace, 38 frd unit tests and 91 surrounding
runtime/transport tests. Runtime checks rebuild current first-party sources with
the pinned compiler and the matching retained Asupersync dependency libraries;
they are not a fresh full Cargo dependency build. Full-workspace Cargo verification
is separately blocked by unavailable offline rustls source. Strict selected
Clippy, repository formatting and documentation checks pass.

This is a persistent host coordinator, not the complete desktop application.
The native Driver, real platform consent/visibility/lifecycle adapters, viewer
input/event-loop integration, qualified clock exchange and live Tailscale ingress
remain application responsibilities. No dependency, release pin, codec, runtime,
listener, permission policy or capability advertisement changes are included.
