# Automatic observation-host recovery

`StreamingHost::enable_reference_recovery` retains the original completed media
attachments for an observation-only session. Native publisher bootstrap enables
it when `reference-recovery` v1 was positively selected. Controlled and
control-requesting sessions do not enter this path; there is no implicit input
grant, replay, reconnect, or replacement capture process.

The normal host loop now consumes recovery requests from the original reliable
control stream before admitting another media send. The existing subscription
performs bounded full-view validation, fences input/readiness and old packets,
and charges its existing recovery allowance. One non-cloneable demand survives
while the one already-issued native capture completes; its obsolete result is
released without entering the sender or being relabeled as recovery output.
No authority or source-scheduler lock spans native IPC or an await.

After native ownership returns, scheduling rechecks the actual source and cache.
The existing replacement owner retires the old media set and attaches three fresh
roles using distinct one-use tickets. The original capture scheduler supplies the
force-IDR request; its rate allowance is not bypassed with an unconditional force
flag. The original sender is rebound, preserving its chronic-failure and repair
histories. Existing host decoder startup checks the actual length-prefixed HEVC
configuration and waits for Configured before releasing the IDR, then for the
matching FirstDecoded before resuming ordinary dependent frames.

Every stage retains the original local failure deadline, including capture drain,
attachment, scheduler delay, capture, configuration and peer decode. Consent,
observation renewal and unrelated application records continue on the original
session. A completed native operation does not cancel an unfinished network turn.
Dropping or cancelling the outer serve operation fences authority before native
cleanup. Failure is terminal for this single-source session, not an excuse to
allocate a fresh sender or replenish its recovery allowance.

Old advisory reports are discarded during handoff. New feedback/presentation
owners begin with unknown evidence; only lifetime report counters survive.
`Statistics::recovered_streams` counts completed attachment/decoder handshakes,
not physical visibility or input authorization. The original local stop handle,
connection proof, capture worker and viewer decoder survive successful recovery.

## Validation

Eight new tests cover real capture-in-flight admission, foreign source refusal,
source retirement, drain expiry, both canonical peers recovering and then running
past the original observation lease, a hung capture, refusal of a dependent
picture despite force-IDR, and missing negotiation/control-intent exclusion.
Together with seven existing admission-policy tests, all 15 focused tests pass.
The end-to-end test deliberately drops a real sender reference and waits through
receive silence past its usefulness deadline; otherwise selective repair correctly
repairs the loss rather than entering recovery. It uses actual TLS/UDP and child
process IPC, with canned codec parameters and synthetic decoding, not hardware or
live-tailnet evidence.

All eight first-party libraries were rebuilt with nightly-2026-08-31 against
unchanged compiler/lock-matched upstream libraries from checksum-verified GitHub
run 35461102442 (source ea73e284). Concurrent cd961a4 admission and 7127c9d
source-scheduler changes are preserved; daemon source and tests were rebuilt after
reconciliation. Strict Clippy checks the complete daemon test source. Runtime
registration was selected in a temporary copy after full daemon code generation
exceeded the execution limit; a separate unrestricted code-generation attempt
was killed with SIGKILL. Production code and selected assertions were not changed. This is not a cold full-workspace Cargo or complete daemon runtime pass.

Real-HEVC/hardware injected-loss qualification, safe controller reacquisition,
shared-encoder fanout, and broader transport qualification remain separate work.
The fixed transport namespace currently allows one complete three-role media
replacement; later exhaustion refuses rather than recycling tombstones.

Refs: plan 7, 11, 12, 17 and 19; fr-p1-loss-recovery-20s.

## Cancellation-ordering reconciliation

The automatic host implementation that landed while the earlier host patch was
awaiting publication is retained. Its original source scheduler, attachment
handoff, sender history and publisher integration are not replaced with the
older parallel implementation.

Recovery now also fences authority inside each native-work scope on failure.
The outer serving guard alone is insufficient for an inner error: a pinned
native future can be dropped while that error unwinds, before the outer caller
observes it. The failure guard is deliberately declared after the protected
futures, so it revokes observation/input authority before their destructors run.
A successful old-capture drain or recovery-capture handoff disarms that guard;
it does not terminate the continuing session or create a new authority grant.

Four regression tests inspect authority from the native future's destructor.
They cover an already expired budget, application permission refusal, a network
failure after native polling, and cancellation during an in-flight handoff.
Three fail against the original production implementation and all four pass
with the fence. All four existing automatic-host tests also pass, including
both canonical peers recovering on their original workers and continuing past
the initial observation lease. Test admission uses actual TLS/UDP; the destructor
witness is a deliberately synthetic pending future, not a codec qualification.

These eight tests were run with the pinned compiler against the checksum-verified
9f1e141 source plus this reconciliation. All eight first-party libraries were
rebuilt using unchanged, compiler-matched upstream artifacts from run 35421807938.
Strict Clippy checks the complete daemon unit-test source; changed Rust files
pass formatting and whitespace checks. Runtime test registration was limited to
these eight tests in an external source copy; all production code, test helpers,
and selected assertions were retained. No repository test was ignored or
weakened. This is not a cold dependency build or a full-workspace runtime pass.
