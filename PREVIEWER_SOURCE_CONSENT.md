# Local source ownership before the first viewer

`SessionAgent::start_shared_display` now registers the original selected source
with its local permission/revocation owner before returning the ordinary shared
session startup future. It reuses the existing display selection, attachments,
shared IDR, decoder handshake and capture worker. It creates no observation grant,
OS permission or input authority. The caller must already have an independently
authorized source and feed actual platform events/probes to the local agent.

Previously, source-consent registration required an existing subscriber's binding.
An independently selected native source therefore could not attach its local revoke
owner until after its first remote admission. A registration can now bind the exact
original selected catalog revision, display and codec configuration before any
viewer exists. It remains bound to the same publisher allocation as the first
viewer establishes the immutable cohort binding. Generic nonselected sources still
require their existing admitted scope; no numeric alias is a transferable owner.

The managed entry point rechecks local permission and unique registration
synchronously. A denied attempt revokes the joining viewer before its first poll.
A retry by the same agent reuses only its original registration, without resetting
renewal cadence, deadlines, source identity or the native process. Another agent
cannot take it over. Failed permission revalidation on an already-owned source
fences that source as well; an unrelated denied agent cannot revoke its owner.

The returned future does not borrow or keep the agent alive. The original local
event loop can service consent renewal, handle lock/session-change/permission-loss
events, or destroy the agent while network startup waits. Local indicator revoke
and agent drop fence the source before another write; media cleanup remains with
the original publisher and its reap operation. No policy lock covers caller nonce
generation, native work or network awaits. Late viewers retain the existing
`HostSession::join_shared_display` path rather than creating another source owner.

The source keeps its original two-second unused-publication budget. Permission
renewal cannot extend it, and the next local maintenance deadline is clamped to
that earlier boundary. After a viewer starts, only fresh local consent checks renew
the source. Peer session renewal remains independent. Source permission renewal,
decoder completion, visibility and input authority remain separate; none invents
pixel progress or dummy video. The eight source registrations and eight subscriber
slots retain their original bounds.

## Executed validation

Fourteen new tests pass, including the managed entry point, same-agent retry,
foreign-agent rejection, synchronous permission refusal, unpolled cancellation,
local revocation during ticket generation, lock/logout/permission loss, agent
destruction, immutable unused-source deadline, nonce failure/unwind and callbacks
that revoke the source without deadlocking. A continuing selected source crosses
its original five-second observation expiry only through local renewal while the
original viewer session renews separately; the same child and single idle picture
remain in use. The first new registration assertion fails with `WrongScope` on
the original implementation and passes with this change.

The final targeted run is **101 passed and one unresolved failure**: all 38
canonical shared-session tests (14 new), all ten shared-late-join tests, all eight
local-source tests, and 45 of 46 shared-startup tests. The startup failure is the
previously retained
`a_delayed_configured_reply_cannot_renew_the_original_shared_frame_deadline`:
its link helper unwraps transport `Expired` before the intended assertion. It
also failed on the unchanged 16cd6e8d baseline in the prior session; the saved
feature revalidation earlier in this session passed all 46. Neither that timing
failure nor the earlier intermittent slow-newcomer timing failure is claimed
resolved. Their assertions and production deadlines remain unchanged.

The combined session target uses the previously required 16 MiB test-thread
stack. All 14 new tests also pass on the default test stack. Production daemon
and the complete actual Linux daemon unit-test source pass strict pedantic Clippy.
Changed-file formatting, whitespace and repository documentation checks pass.
An initial long-running test incorrectly requested a 30-second worker Deadline;
it now uses that existing API's five-second maximum, with no authority-lifetime
or assertion relaxation. The saved shared-bootstrap module's wildcard import was
made explicit to satisfy production Clippy, without changing its behavior.

Execution rebuilds first-party libraries from the exact published 7f53f4c tree
plus this slice with nightly-2026-08-31. Unchanged external libraries are the
compiler/lockfile/checksum-matched retained inputs from GitHub run 35461102442.
The focused runtime harness omits unrelated test registrations in an external
copy; production code and all selected assertions are unchanged. Full test-source
Clippy uses the actual repository. Tests exercise real TLS/UDP and supervised
child-process IPC; permission state, monitor replies, codec payloads and decoder
acknowledgements are explicit fixtures, not native OS/HEVC, GPU, physical-display,
independent-interoperability or live-tailnet evidence. No full-workspace or cold
external-dependency build pass is claimed.

The platform event-loop/listener and automatic OS share-session registry remain
unfinished. This completes the local-owner handoff into shared-display startup,
not an installable workstation or a broader qualification gate. See
[Shared display bootstrap](SHARED_DISPLAY_BOOTSTRAP.md) and
[Shared source consent](SHARED_SOURCE_CONSENT.md). Refs: plan 7/11/19,
`fr-p1-frame-pipeline-am1` and `fr-p2-viewer-admission-e62`.
