# Native control bootstrap: selected scope and terminal handoff

The public `publish_controlled_display` and `observe_for_control` entrypoints
prepare the existing native publisher and observer for explicit control. They
retain the discovered source, selected display, original connection, four
completed channel attachments, decoder and genuine initial completion. The
subsequent `serve_accepting_control` / `serve_requesting_control` operations
perform the existing live grant exchange. This document describes the additional
selected-scope and handoff guarantees, not a completed desktop shell.

## Selected display is a persistent boundary

`NativePublisher::serve_accepting_control` checks a pending or active request
against the actual selected display before passing its approval capability to
local application code. It also checks every nonempty target returned by that
local callback. The display binding, signed desktop rectangle, display geometry,
viewport, codec configuration and recovery generation must remain exact.
A callback cannot silently redirect the native owner to another desktop region
while preserving numerically equal channel IDs. A mismatch is `TargetChanged`
and follows the existing terminal revoke and cleanup path.

`NativePublisher::control_target` derives the correct metadata from the selected
display; `NativeObserver::control_request` derives the corresponding request.
Neither helper probes platform capabilities or grants permission. The application
still supplies actual supported operations and explicit local consent. The
canonical broker independently checks admission, target equality, media readiness,
Seat ownership and native initialization before publishing a grant.

The viewer separately confirms the actual coordinate mapping and independently
observed visibility. Receiving a grant or completing the first decode does not
supply either condition. Qualified unchanged-source observations can maintain a
static view without encoding or decoding another frame.

## One owner, bounded handoff

The native observer keeps the large streaming receiver/transport owner in one
stable allocation, as the publisher already does for its streaming host. Moving
the public handle does not copy that storage through each enclosing future's
`Poll` result and does not clone a connection, clock, frame or authority owner.
The regression suite bounds both public handle sizes below 4 KiB and exercises
the pre-negotiation viewer overload through a distinct host observation-approval
operation. This is a handle-layout and composition guarantee, not a throughput
or physical-display performance measurement.

Both bootstrap and grant-request deadlines include time before the first poll.
The service wrapper constructs the existing request immediately; it does not
start a new timeout inside a later async body. Abandoning prepared service
futures closes the original session, and those handles cannot be reused for
an implicit reacquisition. An existing session clock is refused rather than
replaced with a competing estimator.

The lower-level service returns a native Driver that must be polled independently
of capture and decoder work. The [managed native service](MANAGED_NATIVE_CONTROL.md)
owns and services it instead, including bounded shutdown draining. Source
staleness fences input even while a capture process is stuck.
Releasing a held key is cleanup, not rollback of its earlier press and not a
second action receipt. Authentic results remain available through the existing
`collect_after_close` and `last_result` accessors. Worker reaping uses a separate,
live cleanup context.

## Verification scope

The additional changes were applied to exact upstream source tree
`f0c5facb033749c52d9852f27bc721404505e86a` (commit
`3db207188854a892a12ff0e45ab95eb81541199a`), preserving its public bootstrap APIs
and newer scrolling implementation. All seven first-party daemon/dependency
libraries were rebuilt from these current sources with the repository-pinned
nightly-2026-08-31 compiler and compiler-matched pinned third-party artifacts.
This is not a cold complete native workspace build or committed-source CI result
for these additional changes.

The complete daemon suite passed 202 tests with four threads; 16 pre-existing
native/namespace cases remained ignored and are not counted as passes. The
15 native-control cases, including nine new cases, also passed with one and
eight threads. Strict first-party library and daemon-test Clippy, changed-source
formatting and documentation checks passed.

New coverage includes the complete pre-negotiation approval path, bounded handle
layout, missing mapping after a real grant, original wrapper deadlines, abandoned
service reuse, duplicate clock refusal, changed coordinate/generation refusal,
and a stalled capture while a key remains held. The stalled-capture case records
one press and its independent cleanup release before the one-second capture
watchdog, with exactly one real protocol action receipt and no extra decode.

UDP/TLS, channel negotiation, protocol grants, source-age enforcement and child
process supervision are real in these tests. Monitor inventory, codec replies,
visibility, local consent and the counted native input sink are explicit fixtures.
No hardware HEVC, physical input, physical scanout or live-tailnet qualification
is claimed. The parent broker/client/input beads remain open for their full
platform acceptance gates.

```sh
cargo test -p frd --lib session_startup::native_control --locked -- --test-threads=1
cargo test -p frd --lib session_startup::native_control --locked -- --test-threads=8
cargo test -p frd --lib --locked -- --test-threads=4
./scripts/verify.sh docs
```
