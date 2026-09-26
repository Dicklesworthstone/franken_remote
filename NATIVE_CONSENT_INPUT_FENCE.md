# Local consent retires remote input before a window exists

`ApprovalUi::new` binds the canonical input `Seat` of a control-enabled
`SessionAgent`. Before starting any native consent-window work, it inhibits new
input owners and stops the installed lease with `Suspended`. Consent waits for
the original executor's successful cleanup **and complete destructor**. If that
cannot be established within the existing construction-time two-second mapping
budget, the request is denied with `InputCleanupExpired`, without creating a
window. Mapping consumes the remainder of that same budget; progress cannot
renew it. Old control is never automatically resumed.

## Separate protections, original ownership

The Seat barrier serializes reservation exclusion and increments a monotonic
admission epoch. A broker that reserved before consent cannot start that old
grant afterward, even after the prompt disappears. It stops the installed
Control outside the Seat mutex. Up to eight overlapping non-cloneable guards
are bounded and independent. Unknown cleanup remains occupied; dropping a
guard neither clears that uncertainty nor rewrites already-issued receipts.

The inhibition guard moves into the original approval thread, not its public
status handle. It remains held through native window teardown even when the
Prompt or ApprovalUi is dropped. A stuck foreign call retains exclusion instead
of allowing a new owner to inject into a still-live consent surface.

The existing XI2 rejection of controller-generated consent remains independent.
The newer native cross-process gate from 06986618 is also preserved: positive
consent holds its exclusive kernel permit before window creation, and X11Pointer
holds a shared permit across non-release preparation and submission. This Seat
layer additionally fences reservations and waits for native destruction before
invoking that native gate. Whole-seat exclusion covers keyboard, pointer and
cleanup ordering without relying on a moving window rectangle.

## Reentrant cancellation must not deadlock

Approval startup reserves one pending original capability, then releases the UI
slot mutex before invoking `Seat::inhibit` and the installed executor-stop hook.
A hook may cancel the UI synchronously. Stop marks the slot closed and denies
the pending capability outside the mutex; a concurrently completed prompt stays
collectable. Competing requests cannot replace the reservation. Failure or unwind
retires only that pending capability, without creating a queue or retry.

This corrects a defect in the saved, unpublished UI implementation, which held
the slot mutex during the input-stop callback. A regression uses the real native
input owner and original TLS/UDP approval exchange, checks lock availability
without hanging the old implementation, and performs a genuine reentrant local
stop when unlocked. The old implementation fails; the corrected one passes.

## Integration limits

Configure the agent's ControlProfile before constructing ApprovalUi. Applications
managing the canonical Seat separately must use `ApprovalUi::with_input_seat` or
`Prompt::start_with_input_seat`. A different Seat cannot replace the control
profile's original owner: `WrongInputSeat` refuses before native work. Creating
a new Seat just for the prompt does not protect another input owner. Unscoped
Prompt::start remains for integrations without independently managed input.

This does not implement the separate session-agent process or enable `frd run`'s
local-approval deployment mode. The native gate requires cooperating owners with
the same UID and filesystem namespace; a split PrivateTmp namespace needs the
same gate directory exposed. Neither layer authenticates physical users nor
sandboxes the X server or arbitrary same-user X11 clients. The existing native
exclusion implementation is retained unchanged in this commit.

## Executed verification for publication

The complete input-agent integration target passed **16 tests**. The complete
native library target with linux-input, linux-session-ui, linux-logind and
linux-local-approval passed **46 tests**, including five consent-barrier cases
and all existing approval/logind/mapping tests. Neither run skipped tests.
The 12 agent-bound approval cases also passed separately with the current native
gate. These overlapping runs are 62 unique passing tests, not 74.

Native tests use the actual X11Pointer/XTest sink and consent window alongside
real TLS/UDP Host/Viewer exchange. Identity, private login1 metadata, device-
attributed Xvfb clicks and destructor timing remain explicit fixtures. Eight
first-party libraries were rebuilt from checksum-verified source 284b3d6 plus
this slice using nightly-2026-08-31 and unchanged matching external CI libraries.
The current X11 input, consent C, gate and helper files were reconstructed to
exact Git blob hashes from main 620b8305 before the final native run.

The first broad native run failed after the environment's virtualenv Python took
about 0.7-0.9 seconds per helper launch: the synthetic-input test's original
six-second approval expired before its final helper finished. That failure and
the resulting poisoned-lock failures are retained in the logs. Selecting the
standard /usr/bin/python3 via PATH allowed all 46 unchanged tests to pass. No
approval deadline, assertion, production policy or test registration changed.
The new reentrant-stop regression also has retained failing-before/passing-after
logs. Three reservation unit tests were verified in the earlier saved session;
they were not rerun or counted among this publication's 62 tests.

Strict pedantic Clippy passed for the affected frd/native production libraries,
input-agent integration and complete native library test target. The current
C boundary compiled with -Wall -Wextra -Werror; pinned rustfmt and source-hash
checks passed. This is scoped native/session evidence, not a cold dependency
build, complete latest-main workspace, installed session-agent, physical-device,
GPU or live-tailnet qualification. The broader bead remains open.

Refs: plan 2.1, 7, 15.2 and 19; fr-rc-sec-approval-synthetic-input-t2r.
