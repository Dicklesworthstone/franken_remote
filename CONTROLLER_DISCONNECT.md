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
