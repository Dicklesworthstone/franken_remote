# Control-lease renewal over native QUIC

Control challenge/response now connects the viewer's active input owner to the
canonical native input lease. It shares the exact observation authority instead
of keeping a second copy of its deadlines. Native calls, receipt backpressure,
and ticket issuance do not occupy the renewal path.

This renews an **already locally granted** control lease. It does not grant or
reacquire control, supply local approval, claim that a frame is visible, or turn
transport traffic into capture freshness. The broker's initial control-grant
coordinator and the complete desktop event loop remain separate integration work.

## One authority, one native lifetime

`InputSession::from_shared_authority` and `ObservationControl::input_session`
attach to the same `SessionAuthority` used by observation delivery and renewal.
Attachment requires the existing matching lease, usable ticket and ready view;
it cannot create any of them. `Seat::start_admitted` remains the admitted native
factory entry, and the OS share-session must retain its single Seat until cleanup
and native destruction complete. All participants use the same retained runtime
clock domain.

A shared authority permits one native owner per explicit grant. That claim remains
consumed after the owner stops or is dropped, so a second attachment cannot reset
its input replay ledger or remove its revocation fence. A new explicit grant gets
a distinct retained identity: even deliberately reused numeric lease and ticket
IDs cannot revalidate an old monitor or renewal handle. This local ownership
check does not replace unpredictable, non-reused wire identifiers or the global
Seat's cleanup/handoff checks.

`InputSession::take_control_lease` transfers one non-cloneable, renewal-only
capability. It cannot renew observation, issue tickets, change readiness, submit
native effects, or certify cleanup. The Agent retains it for one attachment;
dropping the Agent or the attached renewal owner fences the native input owner.
The native watchdog and release-only cleanup remain independently driven.

The shared mutex covers pure policy transitions only. No native preparation,
submission, cleanup, network I/O, nonce generator or user callback runs under it.
A participant checks its own clock for regression; a pre-lock sample overtaken
by another participant is serialized conservatively at the newer authority time.
Native ticket metadata is returned with the same effective issue time used by
policy, not an earlier timestamp sampled before lock contention.

## Host control-stream attachment

`QuicInput::control_renewal` takes an `ObservationControl`, the actual original
`QuicRecords` connection, and its authenticated `ControlRoutes`. It verifies
server role, installed critical-priority reliable session-control routes, message
bounds, original connection identity and shared-authority identity before taking
the native capability. Equal identifiers on a different authority or connection
do not substitute; a second renewer cannot take the capability again.

The resulting `ControlRenewal` owns one fixed-size pending challenge and services
one-second issuance cadence. It calls the host nonce generator only when a new
challenge is due. The generator must provide unpredictable, never-reused nonces.
There is no new native command queue, background executor, task or runtime.

The existing authority codec is reused unchanged: a control Challenge is 82 bytes
and its response is 74 bytes, including the FRD0 header. Both contain the exact
session, control scope and lease. Observation messages still use their separate
scope and are left for the observation renewal owner on the SAME control pair.
No new capability or auxiliary stream is silently installed. Input tickets and
actual action results keep their existing input-feedback route.

`service` retains the exact nonce, bytes, issue-time expiry and original send
window under real transport backpressure. The send window is bounded by one
second and both the previous live lease and the issued challenge. Enqueue success
is not renewal. Only a matching response to a queued challenge renews the still-
live lease, to the deadline fixed when the host issued that challenge. A late
response never starts a new three-second duration, and expired authority cannot
be resurrected. Observation renewal or ticket traffic alone cannot extend control.

Every service/receive turn and transport authorization checks current observation
and control permission, the native owner's exact retained admission gate, parent
cancellation, native revocation and the retained host clock. These in-memory
checks do not wait on LocalAPI I/O or a blocked native mailbox. FIN/RESET is read
from authenticated stream-terminal metadata even when native work is pending.
Failed or abandoned I/O stops input and closes only the matching old connection;
dropping an unpolled drive future is covered by the same guard.

The containing session must service observation, control and input fairly, on
idle turns as well as packet arrivals, and poll the native Driver independently.
The [persistent controlled host](CONTROLLED_HOST_SESSION.md) now composes those
service owners, including during admission refresh, without another native queue.
Each receive handler leaves unrelated messages unread when its delegate reports
backpressure. The low-level ControlRenewal adapter starts no autonomous task.

## Viewer lifecycle and view gating

`InputClient::enable_control_renewal` binds a single responder to the existing
input owner's immutable session and lease. `accept_control_challenge`,
`pending_control_response` and `control_response_sent` check the current mapped,
trustworthy presented view. `PresentedInput` additionally checks its actual media
tracker and receiver lifetime, including immediately before exposing pending
response bytes. A late callback, fresh ticket, or queued challenge cannot reopen
a stopped, hidden, unfocused, suspended, stale or disconnected input owner.

The host deadline is opaque to this responder. Its one-second local response-send
deadline counts local backpressure, never grants host authority, and never slides
when a send is retried. The caller sends the exact pending bytes on the same
reliable control stream and marks them sent only on transport acceptance. Scope,
lease, channel, nonce replay, deadline regression and malformed records are
checked. No response consumes an input action/pointer sequence, changes remembered
held keys, overwrites an action receipt, or creates a new ticket.

Ticket-only expiry deliberately does not prevent a fresh, otherwise-live viewer
from responding to control renewal. New input remains paused until a genuinely
valid ticket arrives; control renewal alone cannot bypass that check.

## Verification and remaining scope

The initial five-file shared-authority core published as `0358a13` passed the
complete pinned-workspace fast and documentation lanes in GitHub run
`34475151510`. The final combined integration is verified separately before
publication; its commit identifies that exact run and source objects.

Twenty-nine tests were added across the core and integration increments: eleven
shared-authority/native-lifetime tests, seven client/view-lifetime tests, seven
real X11/QUIC integrations and four blocked-native network/lifecycle tests.
The local selection passes 181 tests with no failures or ignored cases: 84 core,
60 client, 29 X11/QUIC input and eight blocked-native lifecycle cases. Strict
first-party and selected-test Clippy and formatting pass. Local runtime checks
rebuild first-party sources using the pinned compiler and matching retained
Asupersync dependencies; they are not a fresh full Cargo dependency rebuild.

The X11 integrations use actual localhost UDP/TLS, the canonical native owner,
XKB/XTest and private Xvfb servers. Observation/control renewal and native ticket
rollover run together beyond the original three-second lease, while real Shift
and drag state stays held and subsequent releases succeed. Other tests exercise
withheld control replies despite live observation/ticket traffic, unchanged
issue-time deadlines after delayed replies, replay, genuine send backpressure,
foreign owners and abandoned network I/O.

Separate network tests deliberately block test-only native preparation or an
already-entered irreversible call while renewing both authorities past initial
expiry. Revocation, FIN and RESET do not wait for that native call. A returning
preparation submits no effect; an entered submission keeps its actual result and
then undergoes release-only cleanup. Handoff never becomes safe before cleanup.
These injected gates are not measurements of a hung physical OS or GPU.

Removing the one-time native claim or the retained identity check in separate
negative-control copies makes the corresponding unchanged regression fail. The
production assertions, deadlines and source are not relaxed to obtain a pass.

Admission metadata, initial grants, clock and presentation inputs in the native
integration are explicit fixtures. These results do not qualify live Tailscale
policy/ingress, local consent UI, physical display freshness or an installable
remote workstation. No dependency, Asupersync release pin, alternate runtime,
codec, listener or permission policy changes are included.

```sh
cargo test -p fr-core --test shared_control --locked
cargo test -p fr-client --test control_renewal --test presented_input --locked
cargo test -p frd --test input_quic_lifecycle --locked -- --test-threads=4
cargo test -p fr-native --all-features --test input_quic --locked -- --test-threads=4
./scripts/verify.sh fast
./scripts/verify.sh docs
```
