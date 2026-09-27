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
