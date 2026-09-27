# Controller closing on the original connection

`QuicRecords::close_control_with_request` extends the existing closing-only
exchange to a locally fenced input lease. It accepts either the exact lease's
`LeaseRevoked` or the session's `Closed`, stopping at the first valid terminal
record. `ControlCloseOutcome` keeps the lease report separate from `CloseOutcome`;
neither is translated into the other or into an independently confirmed cleanup.
The lease ID must come from the actual input owner, never a peer-proposed field.
A zero lease, wrong session, wrong lease or malformed stage cannot become evidence.

The caller must stop input admission and local native capture before calling.
When doing so cancels the application context, an independently provisioned
cleanup context on the same runtime clock can service the exchange; cancellation
is never cleared or cloned into a new grant. The immutable destination/security
gate still applies. This is not a general post-revocation transport loan.

The original control parser, unread remainder, consumed offsets and fixed
connection receive window are transferred. No media/input callback, challenge
response or application send runs during closing. The existing 32-record bound,
250-ms construction-time maximum, earlier caller deadline, new-stream refusal,
and native retained-payload/partial-send/datagram refusal are unchanged. A foreign
connection proof is nonmutating; every matched attempt closes ordinary I/O at
call time, including invalid and abandoned attempts. Request acknowledgement,
report fields and transport completion remain independent.

Individual input receipts are still owned by the original input ledger. The
exchange does not replay pending actions, drain arbitrary input lanes, infer zero
outstanding effects, or claim remote keys were released because bytes were ACKed.
An absent final report remains absent. Native input cleanup must run through the
host's original independent input owner, not through this report decoder.

## Verification

Six new actual TLS/UDP integration tests pass. They cover all cleanup/effect stage
combinations, the two possible terminal report families, exact lease/session
binding, malformed records, competing queued reports, foreign proofs, native
backlog, cancellation, security loss and delayed polling. The complete existing
terminal suite also passed (42 cases), as did the two closing receive-credit
regressions. Cleanup/effect fields are explicit fixtures, not X11/input-release
qualification. Strict pedantic production and new-test Clippy and formatting pass.

The executed source baseline is checksum-verified 63caea4, whose modified files
remain unchanged at current main b2a8d8a. First-party dependencies were rebuilt
with pinned nightly-2026-08-31 against matching unchanged external libraries from
CI run 36291188001. This is not a cold dependency or complete-workspace run.

This first slice is the transport primitive. Controller session/window shutdown
must join its original input fence, capture cleanup and receipt retention before
selecting it. Existing observation closing and immediate emergency stop are not
changed. No dependency, alternative runtime or protocol kind was added.

Refs: plan 7.3/19; PROTOCOL.md section 8; fr-rc-protocol-refusal-closure-5dx.

## Granted controller ownership

`ControlledViewer::disconnect_with_cleanup(cleanup, reason)` now supplies the
original granted lease and session binding itself. It first freezes the existing
response/silence/pending-record deadlines and an at-most-250-ms call-time budget.
It then closes ordinary session admission, stops renewal, fences input and its
viewport, discards unsent records/events, and invokes the original native capture,
clipboard and file cleanup boundaries. The input fence precedes native callbacks;
even a callback returning late cannot acquire another network-close budget.

The original event source cancels its application context. The separately
provisioned cleanup context is only for the typed closing exchange, never a
replacement input or session authority. Passing the cancelled application
context refuses reporting while still stopping input. Emergency stop and the
existing immediate `close` remain immediate. An abandoned unpolled operation
still closes the original transport and retains no invented completion result.

The outcome is retained by `ControlledViewer::disconnect_outcome` through repeat
close, rejection of a second disconnect, and native capture reaping. A received
session report also remains in the original ViewerSession. Pending action and
last-receipt ledgers survive; neither an encoded-but-unsent action nor a host
cleanup report fabricates an individual action result. The native-capture handle
remains available for `reap_input_capture` and reports Pending until its real
owner confirms completion. Host driver cleanup and local capture cleanup remain
independent of request/report transport acknowledgements.

Six new granted-owner tests pass with the original authenticated connection,
production dispatcher, receipt ledger and independently driven host input owner.
The end-to-end test applies a key press to the explicit fixture sink, requests
close while another encoded action is pending, receives the exact host lease
report, and observes the original Driver's release of its held state. The pending
viewer action stays unresolved. Native-capture lifecycle and host-accounting
alternatives are explicit fixtures, not real X11/key-release qualification.
Other cases cover absent completion on abandonment, the cancelled application
context, delayed polling, native outbound backlog, exact session-report retention,
and a deliberately late stop callback without renewing the network deadline.

All 28 selected controller tests pass (six new and 22 existing event/capture,
renewal, revocation and native-owner lifecycle regressions). Together with the
50 transport tests above, 78 unique runtime cases passed in this session, including
12 new cases. The complete production daemon and transport pass strict pedantic
Clippy; formatting and whitespace checks pass. The complete daemon test-source
Clippy attempt exceeded a 90-second local limit without diagnostics. Runtime tests
were compiled from a separate copy excluding only 724 unselected test registrations;
a normalized comparison verified identical production code and selected assertions.
An exploratory lint of that reduced copy exposed expected unused fixture code and
new large-future/test-structure findings; the new issues were corrected by bounding
the owned terminal future on the heap, splitting assertions and using explicit
result disposal, not by changing deadlines, assertions or repository lint policy.
No complete daemon-test Clippy or current-workspace pass is claimed.

First-party libraries were rebuilt from the same verified 63caea4 baseline and
these changes, with the pinned compiler and matching external CI libraries.
Modified preimages were checked against f0734e01 on main; newer native-window/CLI
work is preserved rather than overwritten or counted as rerun. Automatic
controller window/stream-loop selection, independently confirmed native host
cleanup and final effect accounting remain open. Applications owning a separate
receiver/presenter must retire that media before selecting this control-owner API.

## Running granted-controller shutdown

`StreamingViewer::disconnect_control_with_cleanup` composes the granted input
owner with its original receiver, presenter, audio and repair state. Input is
fenced and the original deadlines are frozen before media retirement; all of
that happens at method call, including when the future is abandoned unpolled.
The original input-capture and decoder owners remain available for explicit
reaping, and externally borrowed compressed pictures retain their byte charges.

`StreamingViewer::serve_control_until` runs the existing granted-controller loop
until its bounded UI callback returns `ControlFlow::Break(reason)`. It prepares
the owned terminal exchange inside that callback turn. This ordering matters:
input is fenced BEFORE the pending decoder future unwinds, not after awaiting
a late result or dropping the native operation first. No further presentation,
input-result or application callback executes during the exchange. Emergency
stop and callback/protocol errors remain immediate failures, not local intent.

`control_disconnect_outcome` and `pending_control_actions` continue reading the
original input owner after teardown. Closing does not replay unsent actions,
erase pending receipts, translate a session report into lease release, or infer
local native completion from transport success. A rejected repeat attempt cannot
erase an earlier report. The independently supplied cleanup context has the same
requirements as the granted-owner API above; using the cancelled application
context refuses reporting without delaying the local input/media fence.

Six added running-controller tests and 24 existing controller/observation/audio
tests passed (30 distinct runtime cases). Actual TLS/UDP, production input/receipt
ledgers and supervised decoder IPC run with explicit compressed-picture, native
input-capture, visibility and host-accounting fixtures. Media bootstrap is
injected into the production receiver; closing requests and terminal reports
cross the original authenticated connection. The new cases cover direct and
UI-initiated closure after presentation and during pending decode, abandonment,
exact report retention through both native reaps, external buffer charging,
unresolved actions, emergency/callback distinction and wrong cleanup contexts.
The first run correctly refused a new test action without a visibility witness;
the test now supplies that explicit fixture witness without changing production
gates or assertions. The complete selected rerun passed.

All eight relevant first-party libraries were rebuilt from the checksum-verified
b6aca68 source using pinned nightly-2026-08-31 and unchanged matching external
libraries retained by CI run 36335317709. Complete production-daemon AND complete
daemon test-source strict pedantic Clippy passed. New and existing test-only lint
findings were corrected with explicit empty-array assertions and a boxed composed
future; no check, timeout or runtime policy was weakened. Changed-file formatting
and whitespace checks passed. Runtime
tests use a separate copy excluding only unselected test registrations; a
normalized source comparison verifies unchanged production bodies and selected
assertions. This is not a cold dependency build, full-workspace run, native
X11/input-release, HEVC/GPU or installed-Tailscale qualification.

Interactive acquisition and native controller-window selection still need their
own integration; this API requires an actually granted controller. Observation
closing and immediate window/emergency behavior remain unchanged. Independently
confirmed host cleanup and final external-effect accounting remain open.

## Interactive acquisition through the same closing owner

`StreamingViewer::serve_interactive_control_until` and the public
`NativeObserver::serve_interactive_control_until` now keep watching, requesting,
actual grant, input delivery and orderly closing in ONE original service. They
share the previous interactive path's role/attachment validation and one-way
receiver/view-history transfer. The existing `serve_interactive_control` remains
source-compatible. No second service invocation, replacement receiver/decoder,
clock reset, duplicate bootstrap decode or early control-request timeout is used.

The local callback returns `ControlFlow::Continue(())` or an explicit
`Break(reason)`. After a real grant, Break freezes the original deadlines and
fences its input INSIDE the callback turn, before a pending decoder can unwind;
media is then retired before polling the typed terminal exchange. The original
cleanup context must be independently provisioned. A cancelled application
context refuses reporting rather than being reset into new authority. Before
an actual grant, Break cancels locally with `Closed` and no controller report;
a just-staged request cannot escape that turn or manufacture a lease. Native
window emergency events, callback errors, lost authority and protocol failures
keep their existing immediate behavior instead of being relabelled local intent.

The native wrapper exposes `control_disconnect_outcome` and
`pending_control_actions` on the original controller after teardown/reaping.
The last authentic receipt remains readable. An unsent release stays unresolved,
even when the original managed host Driver releases its held key during cleanup.
Host lease stages, session reports, request/transport acknowledgements and actual
local native collection remain separate evidence.

Five additional tests run public host/viewer bootstrap and the existing managed
host service; they do not inject a granted viewer or manually arm its reporting
owner. The central case watches without a reserved Seat, explicitly requests and
receives consent, submits a key press and receives its authentic action receipt,
then closes with a release still encoded but unsent. The host Driver releases
its fixture held state and reports its actual lease; the viewer retains its
unresolved release and the exact report through native reaping. Other cases
cover pre-grant closure (including a request staged in that same turn), abandoned
unpolled service, callback/emergency distinction and a cancelled cleanup context.
The original watch-past-request-budget regression and four managed-revocation
regressions pass unchanged.

Final validation in this continuation: 40 selected daemon runtime tests and all
50 existing controller-terminal/terminal-drain/receive-credit transport cases
passed (90 distinct tests, including the 11 added across both commits; reruns
are not counted twice). Complete production daemon and complete original daemon
test-source strict pedantic Clippy passed; changed-file formatting and whitespace
checks passed. Runtime registration filtering in a separate copy excluded only
1142 unselected tests; normalized comparison verifies identical production and
selected assertions. All eight relevant first-party libraries were rebuilt from
the verified b6aca68 source plus these slices with the pinned compiler and matching
unchanged external libraries retained by CI run 36335317709. The new test helper's
initial missing Action lifetime and unused mutable binding were corrected before
runtime verification, not suppressed. Both final daemon runs passed.

TLS/UDP, public startup/consent protocols, receipt tracking, managed input Driver
and supervised source/decoder IPC are exercised. Codec output, platform visibility,
native input capture and OS effects are explicit fixtures: this is not real
HEVC/GPU, X11 input release, installed-Tailscale or a full current-workspace/cold
dependency qualification. Concurrent native test-only fixes are preserved, not
counted as rerun. Selecting this API from the default native controller window
and CLI, independently confirmed host-native cleanup and final effect accounting
remain open. The default controller window close remains an immediate input fence.
