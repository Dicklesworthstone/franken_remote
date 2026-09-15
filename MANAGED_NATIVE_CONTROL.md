# Managed native control service

`NativePublisher::serve_managed_control` owns the native input driver alongside
its existing streaming service. An application no longer needs a separate task
or channel to receive and continuously poll the driver returned by local
approval. The lower-level `serve_accepting_control` remains available for
applications that explicitly coordinate that ownership themselves.

This entrypoint follows the existing `publish_controlled_display` bootstrap.
It does not supply a desktop shell, choose a source, probe platform capabilities,
approve a connection, or upgrade observation intent to control.

## Approval and ownership

The local callback receives `ManagedHostControlState`. Its pending state exposes
`ManagedPendingControl`: the original request, native status, readiness query,
and explicit approve/deny/stop operations. `approve` accepts the same current
qualified target, fresh credentials, native sink factory and native cleanup
callback as the existing broker. It returns `Result<(), GrantError>` instead of
a driver: successful approval immediately transfers that driver into the
service's single private slot. Factories and application callbacks never run
under the slot mutex. An active state retains the original request and local
revocation handle.

The selected-display guard, authenticated connection, exact channel attachments,
local consent, source-anchored presentation readiness, native capability checks,
Seat reservation and initialization all remain mandatory. The viewer still
confirms its actual coordinate mapping and genuine platform visibility. The
managed entrypoint does not synthesize any of those prerequisites.

## Servicing, stop, and cleanup

The service polls the same native driver before and after each bounded
network/media poll, including the poll in which approval creates the driver.
Native calls remain on the existing supervised foreign-call thread. Capture or
decoder waits do not prevent the driver's authority watchdog from progressing.
There is no additional runtime, native worker, input authority or command queue.
Application callbacks must still be bounded and nonblocking; this composition
cannot preempt arbitrary application code or an unpolled enclosing task.

Session failure revokes observation before dropping network work, then drains
the original driver. Conversely, a stopped driver immediately revokes the
session before the next network/media poll. Driver drain continues without
calling application callbacks or restarting its existing one-second deadline.
A healthy network turn is never cancelled just because native work progresses.

The returned `ManagedControlReport` separates `session: Result<(), PublisherError>`
from `input: Option<Shutdown>`. A session error does not erase real cleanup
results. `input == None` means this service obtained no driver, not proof that
no initialization was attempted. A shutdown with `exit == None` is unresolved
native work, **not** a safe handoff. Use the original shutdown's `handoff_safe()`
evidence; returning from this service never releases the Seat itself.

Dropping even an unpolled service fences the original publication. Dropping a
polled service also abandons its driver through the existing release-only
cleanup path without manufacturing a shutdown result or freeing a stuck Seat.
The same handles cannot silently reacquire control. Existing `collect_after_close`
and explicit `reap_media` remain available on the publisher; media reaping uses
a separate live cleanup context. Cleanup never replays an input action or
fabricates another action receipt.

## Verification

Nine new public-path regressions run the host managed service and viewer service
without a separately polled input-driver task. They cover renewal beyond the
initial lease with a held key, capture stall, absent consent, absent visibility,
factory failure, blocked initialization, callback failure immediately after
approval, unpolled cancellation and polled abandonment with held input. The
stalled-capture case observes the key's cleanup release before the capture
watchdog, with one original action receipt and no additional decoded frame.
The blocked factory returns unresolved shutdown while retaining the Seat until
actual native cleanup/destruction completes.

UDP/TLS, protocol negotiation, grants, deadlines and child-process supervision
are real in these tests. Display discovery, codec replies, consent, visibility
and the counted native input sink are explicit fixtures. They are not physical
input, hardware HEVC, physical scanout or live-tailnet qualification. Verification
rebuilds first-party sources on the pinned nightly with compiler-matched pinned
third-party libraries; this is distinct from a cold complete native workspace
build. Parent broker/client/input acceptance beads remain open.

The final local daemon run passed 211 tests serially, with 16 existing native/
namespace cases ignored. Strict production and test Clippy passed. The parallel
qualification remains unresolved: a four-thread native-control run passed 20 of
24 cases, and an eight-thread managed-only run passed three of nine. Failures
included expired initial presentation/source evidence and bootstrap cancellation,
not accepted stale input. Both new and prior cases were affected. These failed
runs are retained; no production deadline, assertion or test gate was relaxed.

```sh
cargo test -p frd --lib session_startup::native_control --locked -- --test-threads=1
cargo test -p frd --lib session_startup::native_control --locked -- --test-threads=8
cargo test -p frd --lib --locked -- --test-threads=1
./scripts/verify.sh docs
```
