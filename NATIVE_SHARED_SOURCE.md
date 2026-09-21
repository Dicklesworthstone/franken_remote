# Locally owned native shared-source preparation

`SessionAgent::prepare_native_shared_source` discovers the actual capture worker's
monitor catalog, accepts one local full-display selection, configures that same
child and captures its first source/pool-bound shared IDR. It returns a Publisher
and the retained initial picture, not a new observation grant or a visible-frame
claim. The launch must identify a local protected package; the source control must
already be independently authorized. Retain `Launch::retain_cleanup`'s Retirement
before handing off the launch, so interrupted startup still has a real child owner.

Preparation occupies one of the local agent's existing eight source registrations
before any native work. Local indicator revoke, permission loss and agent cleanup
therefore fence an in-flight discovery/configuration/capture as well as published
sources. The original renewer claim stays set during the direct reservation-to-
publication transfer; another agent or network renewer cannot seize an intermediate
state. The selected catalog, original authority, child and frame pool are retained.
A borrowed local selection callback executes outside source/registry locks.

One absolute two-second preparation deadline, capped by the original source's
expiry, covers all native stages and time before polling. Preparation does not
renew consent or reset native timeouts. A single timer on the source's existing
Asupersync driver services local events within a cooperative 10 ms bound while
worker IPC is pending. The local adapter must be nonblocking, report actual OS
permissions/events and retain any held-input cleanup. Dropping an unpolled future,
local Stop, failure or caught callback panic revokes authority before releasing
native work, even when a completed/failed future is retained. Original native
refusals survive cleanup instead of being replaced by cleanup cancellation.

## Executed checks

Eight new tests pass on the default test-thread stack. They cover same-child
selection and first-viewer startup, unique local registration, no spawn on
unpolled abandonment or initial permission denial, foreign-owner refusal during
selection, source-slot reuse after cancellation, fixed pre-poll deadlines,
selection refusal, reentrant source revoke, retained futures after selector panic,
and permission loss while discovery or capture is deliberately stalled. Child
retirement is collected through its original supervisor. These tests exercise
production discovery/configuration/capture/publication and real process IPC, but
monitor/HEVC records and permissions are explicit fixtures, not native X11/codec
or hardware evidence. The first-viewer test also uses actual TLS/UDP session owners.

The canonical group initially passed all 85 tests. The final borrowed-catalog API
passed all eight new tests and full production/actual Linux daemon test-source
strict pedantic Clippy; its first complete group run passed 84 and failed the
existing negotiated-feedback timing test. The failure is retained, not hidden by
changing assertions or authority deadlines. The unchanged final feature binary
then passed all 85 tests on a complete rerun; the initial failure is not claimed
resolved. Individual outcomes remain in the evidence. Changed-file formatting and
whitespace checks pass. The new selection-refusal test initially exposed error
masking by cancellation cleanup; the implementation now preserves that cause.

All eight relevant first-party libraries are rebuilt from checksum-verified
1301a6d4 source using nightly-2026-08-31. External libraries match the compiler,
versions and checksums retained by GitHub run 35461102442. The focused runtime
harness suppresses only unrelated test registrations in an external copy; full
actual Linux daemon test source is independently linted. These are not cold/full-
workspace, native OS/HEVC, GPU, physical-display or live-tailnet qualifications.

Preparing this source after the first observer's approval, native permission/UI
adapters and protected listener acceptance remain separate integration work.
Broader beads remain open. Refs: plan 7/11/19; fr-p1-frame-pipeline-am1;
fr-p2-viewer-admission-e62. See [first observer](FIRST_SHARED_OBSERVER.md).
