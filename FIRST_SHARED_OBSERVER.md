# First observer under the original local source owner

`SessionAgent::open_shared_desktop` accepts the first protected TLS/tailnet-admitted
Host of an independently authorized, locally selected Publisher. It retains the
original Host, source registration and bootstrap picture through negotiation,
local approval, display choice and media attachments. It returns the original Hub
with its pending decoder handshake, ready for `serve_shared_desktop`; success is
service ownership, not first decode, visible presentation or input authority.

Previously the managed service required an established first SharedHost. The new
bootstrap services the required bounded nonblocking local-event callback and the
original source-consent registration while the first remote admission waits. OS
permission probes/events must still be supplied by the platform. Local consent
renewal cannot be replaced by a peer heartbeat, and the callback remains responsible
for any held-input cleanup returned by local lifecycle operations.

The original Host deadline caps all following display/attachment/decoder startup.
The source's original two-second unused-publication deadline also remains in force;
permission renewal or an approval notification cannot extend it. Source preparation
therefore belongs near admission, not indefinite warm-idle capture. The bootstrap
picture retains its real timestamp, source/pool identity and reference deadline.
No capture occurs through this path before a subscriber exists.

The cancellation guard is constructed at API call time. Dropping even an unpolled
bootstrap, local Stop, error or callback unwinding fences the original peer and
source before releasing network work. A retained future after a caught panic is
already terminal. Successful transfer preserves the same source registration,
capture child, network owner and entropy supplier. Another local agent cannot take
over or revoke an existing registration. Control-intent requests are refused, not
downgraded. No listener, identity, input grant, visibility evidence, codec, worker,
queue, dependency or runtime is added.

## Executed checks

Seven new real TLS/UDP/child-IPC tests cover approval waiting followed by continuous
service past the peer's initial three-second lifetime, unpolled cancellation,
local Stop and permission loss before notification, control refusal, the immutable
unused-source deadline, caught local-callback panic with retained future, and
foreign-agent refusal without revoking the original owner. All seven pass separately
on the default test-thread stack. The complete canonical shared-session group passes
73 tests (seven new and 66 unchanged) on final source using the existing 16 MiB group
stack setting. Production and complete actual Linux daemon test-source strict
pedantic Clippy, changed-file formatting and whitespace checks pass.

Initial tests caught two real distinctions: the continuing-service fixture must
reuse the original entropy supplier rather than regenerate consumed nonces, and a
registration failure must cancel the moved first peer's dedicated context before
returning. The fixture now preserves its supplier and the production failure path
explicitly cancels the peer. No authority deadline or retained assertion was relaxed.
One intermediate combined compile/test invocation was externally timed out; it is
not counted as a completed test run. The final complete group and default-stack new
group both pass.

First-party libraries are rebuilt from checksum-verified 68926978 plus this slice
with nightly-2026-08-31. External inputs match the compiler and every external
lockfile version/checksum retained by GitHub run 35461102442. Remote dfc583cd changes
only beads above that code baseline. The runtime harness omits unrelated test
registrations in an external source copy; production code and selected assertions
remain unchanged. Full actual Linux daemon test source is independently linted.
Identity, permissions, monitor descriptions, codec pictures and decoder receipts
are explicit fixtures. No cold/full-workspace, native OS/HEVC, GPU, physical-display,
independent-wire or live-tailnet qualification is claimed. The previously retained
broader delayed-Configured startup failure is not claimed resolved by this slice.

At this first checkpoint, automatic startup-to-running composition remained separate
work; it is implemented below. Raw native listener, native local UI/event adapters
and first-source creation remain open, as do the broader beads.
Refs: plan 7/11/17/19; fr-p1-frame-pipeline-am1; fr-p2-viewer-admission-e62. See
[incoming observers](INCOMING_SHARED_VIEWERS.md) and
[shared service](SHARED_DESKTOP_SERVICE.md).

## Automatic transition into continuous service

`SessionAgent::run_shared_desktop` now composes first-observer bootstrap directly
with `serve_shared_desktop`. It validates and registers the original selected
source at call time, then retains the same callback state and entropy supplier
across both phases. Callers no longer move an intermediate Hub into a separately
scheduled capture/viewer loop. The original source/Host/picture deadlines still
apply, including time before the combined future is first polled.

A bounded local `announce` callback receives weak admission access and the first
viewer's cancellation ticket exactly once. This exposes the running service to
local application wiring, not decoder readiness, physical visibility or permission.
It runs after attachment but before first decoder completion and can admit another
already-protected Host reentrantly. The continuous service's cancellation guard
exists before this callback runs; its first poll checks local events/consent before
any I/O. One fixed metadata cell lets the outer unwind guard fence even admissions
created by a callback that then fails or panics. It is not a payload queue or a new
authority. A retained terminal/panicked future is already fenced. The original
first viewer's departure does not end a service with another established viewer.
The returned report counts consent renewals during continuous service, not bootstrap.

Four additional tests pass: automatic first admission, late join and continued
service after the first viewer leaves; never-polled cancellation; parking past the
original Host deadline; and callback error/panic/source-revoke after reentrant
pending admission. The latter checks every original peer and ticket before dropping
the terminal future and ensures pending approval cannot run. All 77 canonical
shared-session tests pass. All eleven tests added across the two slices also pass
on the default stack together with the two existing managed-service tests (13 total).
Production and complete actual Linux daemon test-source strict pedantic Clippy,
changed-file formatting and whitespace checks pass on final source.

Broader final-source checks pass eight local-source tests and 45 of 46 shared-startup
tests. The delayed-Configured connection helper still returns transport Expired
before its assertion; a freshly rebuilt unchanged 68926978 baseline has the same
45/1 result. The first late-join group run passes nine and fails its existing
single-network-turn configuration assertion. The unchanged feature binary rerun
passes all ten, as does the fresh unchanged baseline; this does not resolve or erase
the timing failure. Thus the final groups (including that explicit rerun) total
140 passed and one persistent failure, with the earlier additional transient failure
retained separately. No assertions, timeouts or old test bodies were changed.

Execution has the same pinned/compiler-matched source and upstream inputs stated
above, now including the published first-bootstrap checkpoint. These scoped tests
are not a full-workspace or native/hardware qualification. Actual protected listener
acceptance, native permission/UI adapters and creation of the initial authorized
source still remain outside this post-admission service; no broader bead is closed.
