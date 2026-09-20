# Bounded service of shared viewers

`session_startup::shared_viewers::Hub` owns the original `SharedHost` drivers for
one existing capture source. The first viewer and late admissions share one fixed
eight-slot registry (three viewers by default). Pending display selection and media
attachments consume slots immediately, not only after joining. The hub uses each
original authenticated session, UDP driver, renewal, decoder-startup, feedback and
recovery path; it creates no transport, worker, media queue or permission grant.

A weak `Admission` handle accepts already-approved observation sessions in the
same boot/OS scope. The source's selected display is still explicitly chosen by
the viewer through normal negotiation. Duplicate session identities, full slots,
control intent and foreign scope refuse and close the moved session. The existing
join future is constructed at admission time: waiting for service counts against
its original absolute timeout. Failed or abandoned joins cannot restart that
budget. Each slot retains its original future across polls, including incomplete
I/O, while round-robin polling visits every bounded slot without waiting for a
slow peer. Existing connection and media byte limits still govern each original
owner; only fixed registry/receipt metadata and one bounded owner per slot are
added. No payload copies or replay queue are introduced.

Tickets cancel only their original opaque observation owner. Reusing a registry
slot or a numeric remote-session ID cannot make an old ticket revoke a replacement.
A ticket reports Starting, Serving or Finished; Serving means driver ownership,
not decoder readiness, visible pixels or input authority. Local cancellation and
hub teardown fence authority before dropping network futures. Registry locks never
cover a future poll, native work or the supplied entropy callback. Dropping even
an unpolled hub service closes every original member and pending admission.

The source still belongs to `Publisher::serve`, and only the original local
`SessionAgent` can renew source consent. Those services must run concurrently with
the hub; a viewer heartbeat cannot substitute for local consent. The hub is not a
new listener or an OS permission probe.

## Verification

Seven new real TLS/UDP/session tests cover late joining with continuing capture,
slot reuse and stale cancellation tickets, pending-slot capacity, duplicate/foreign
scope and control-intent refusal, unpolled cancellation, fixed admission deadlines,
a silent newcomer while the healthy session renews beyond its original lifetime,
and reentrant entropy callbacks. All seven pass on the default test-thread stack.
The complete canonical shared-session group passes 45 tests with the previously
used 16 MiB test stack. Production and complete actual Linux daemon unit-test
source pass strict pedantic Clippy. Changed-file formatting and whitespace pass.

Retain the initial negative result: the new reuse test stopped servicing its
supposedly healthy client during a rate-limited third join, so the existing sender
reference deadline correctly retired it and the last established source owner.
The fixture now keeps that client consuming its original connection concurrently;
no production timeout or assertion was weakened. A nested fixture future initially
overflowed the default stack; boxing the large test-owned future/result fixes it,
without changing production stack allowances. The final default-stack new suite
passes. Temporary diagnostic prints are not part of the committed code.

All eight relevant first-party libraries build from checksum-verified 8306c6d9
source plus this change using nightly-2026-08-31. External build inputs are the
compiler/lockfile/checksum-matched libraries retained by GitHub run 35461102442,
not a substitute runtime. The runtime harness removes only unrelated test
registrations in an external copy; production code and all retained test bodies
and assertions are unchanged. The actual complete test source is independently
linted. Source/codec payloads and decoder acknowledgements are explicit fixtures;
these checks are not native OS/HEVC, GPU, physical-display, live-tailnet,
independent-wire or cold/full-workspace qualification. Broader beads remain open.
Refs: plan 7/11/17/19; fr-p1-frame-pipeline-am1 and fr-p2-viewer-admission-e62.
