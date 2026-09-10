# Initial broker control grants

`frd::input_quic::grant::GrantBroker` connects the existing `ControlRequest` and
`LeaseGranted` codecs to the OS share-session's canonical native input owner.
It receives a real request on authenticated QUIC control streams, requires an
explicit local decision for the exact target, reserves the shared native seat,
initializes that owner off the authority path, and sends the grant only after
initialization succeeds. The same owner then becomes the existing `QuicInput`.
No alternative executor, input replay ledger or transport is introduced.

## Admission and prerequisites

The caller must retain the original observation/admission owner and the SINGLE
`Seat` shared by every controller contender for this OS share-session. Creating
another Seat is not a way to bypass previous-controller cleanup. Session-control,
ordered-input and input-feedback routes must already be authenticated and
installed on the same connection. `InputFeedback`, rather than a legacy exact
result-only stream, is required so future ticket renewal remains available.

The selected native profile must positively negotiate `native-control-grant`
version 1 with `RequestControl`. The broker checks the complete parent session,
control routes, input binding, role, message limits, original connection identity
and, when present, the retained Tailscale admission's actual local/peer endpoints.
The native owner receives that same control-capable admission gate and timer
context, not a fresh proof or a peer-supplied clock. `HostSession::control_broker`
obtains the binding, selection and observation from the actual running session;
it refuses missing input routes instead of silently installing them.

**Input-route negotiation, initial consent UI and the complete desktop event
loop are not implemented here.** Unlike configuration/recovery/video roles,
input routes are not yet negotiated by the media attachment owner. The direct
broker entry accepts those already installed routes, just as the existing input
and client-grant entrypoints do. Tests explicitly install these routes on actual
UDP/TLS connections. They do not claim that an ordinary bootstrap-only connection
can already open a complete remote desktop.

## Request, approval, initialization and publication

There is at most one fixed-size pending request or one provisional native grant.
Requests consume no Seat, issue no credential and invoke no native factory.
The broker retains the request's sequence floor and a fixed two-second local
approval/startup deadline; a replay or another request cannot replace a pending
request or slide that deadline. This does not extend the viewer's independently
maintained original request deadline.

`request()` is only a copied, validated request for local approval. The locally
authenticated owner must call `approve` with its current exact target after
consent. Requested bounds, capabilities and view generations are not silently
clamped or converted into permission. The shared authority must ALREADY have a
qualified ready view. The broker never calls `mark_view_ready` on the strength of
an approval, request, decoder completion, or a matching target tuple.

Approval atomically reserves the Seat BEFORE invoking the host credential source
or creating any lease. A busy Seat calls neither the credential source nor the
native factory and leaves the losing viewer's observation intact. Before native
launch, an abandoned reservation releases its own slot. After successful launch,
only the native finalizer can release it, after confirmed cleanup AND destruction
of the sink and native closures. A late reservation destructor cannot clear a
successor's seat.

The broker creates the provisional lease/ticket on the SAME observation authority,
then attaches a single native input session. `approve` returns the existing
`Driver`; poll it independently immediately, including during blocked native
initialization. Native preparation, factory code, network I/O and credential
callbacks never execute under an authority mutex. `x11_factory` provides the same
explicit-local-display, bounds/capability-revalidating factory used by `start_x11`.
Creating that closure opens no X connection and submits no input.

`service` requires fresh qualified local target/view evidence on every turn.
It checks the original native identity and ticket again and rejects already
buffered input before publishing any grant. Native initialization is not a grant:
`NativeStarting` exposes no grant bytes. An initialization failure, panic, stale
target, revoked permission, expired ticket or cancelled context cannot produce a
successful grant. A late local `deny()` also fences an already-started provisional
native owner, rather than merely clearing the original request.

Actual QUIC backpressure preserves one exact 209-byte `LeaseGranted`, its native
issue time, and the original exclusive request/ticket/lease deadlines. Successful
enqueue is not evidence of viewer receipt. Expired unsent grants are not reminted.
`finish` moves the initialized owner into `QuicInput` only after transport accepts
the record; finishing early or for another connection/target revokes the provisional
grant. The initial ticket occupies issuance sequence zero, so the same native
feedback owner starts future ticket renewal at ONE with its original cadence.
Neither input action nor pointer positions are consumed by the grant exchange.

## Failure and lifetime boundaries

Peer FIN/RESET, local cancellation, failed I/O and dropping even an unpolled
broker drive future fence provisional input before closing this viewer's
observation. Drive turns are capped at 100 milliseconds. A foreign connection
with equal numeric routes cannot receive the grant, lose its retained queue, or
be closed by the old broker. A native call or destructor that has not returned
still occupies the Seat; revocation is never labelled completed native cleanup.

A denial before native startup retains viewing. A failed or abandoned provisional
grant closes its own remote session; it does not revoke another viewer's separate
observation or cancel the OS-session-owned capture source. The observation permits
one broker attachment for its lifetime; a successful handoff consumes it. This
profile reacquires control with a new explicit session, not a reconstructed old
broker that could reset a request floor or native replay ledger.

## Verification

The reservation increment is published as `4e7dbfe`; its exact source passed the
complete pinned workspace `fast` and documentation lanes in run `34504782712`.
Two new reservation tests complement the existing twelve native-owner tests,
including blocked cleanup and native destructors.

Sixteen new broker tests use actual localhost UDP/TLS and private Xvfb servers.
The success path sends a request, starts the real X11/XKB/XTest owner, accepts the
actual grant in `RequestControl`, separately confirms mapping/presentation, and
submits real Shift/drag/release actions with returned native receipts. The first
renewed ticket is sequence one. Other tests cover concurrent contenders, no
consent or readiness, native initialization failure/panic, blocked initialization
with independent watchdog expiry, late denial, real send backpressure, original
deadlines, foreign connections, replay, early input and abandoned I/O.

Two additional core regressions verify that grant publication checks the original
ticket and retained native identity, rejects a recreated equal-numeric lease,
accounts conservatively for an overtaken clock sample, and cannot restore tickets
invalidated by stale presentation. Initial admission, installed input routes,
qualified view/consent and same-runtime clock samples are explicitly fixtures,
not proof of live tailnet ingress, a consent UI, physical display timing or a
complete application. Dependency and Asupersync release pins remain unchanged.

```sh
cargo test -p fr-core --test shared_control --locked
cargo test -p frd --test input_agent --locked
cargo test -p fr-native --features linux-input-agent --test input_quic --locked -- --test-threads=4
./scripts/verify.sh fast
./scripts/verify.sh docs
```
