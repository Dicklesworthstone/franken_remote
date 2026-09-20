# Shared-source reference recovery

A failed observation subscriber can now recover on its original QUIC connection,
sender cache and native decoder without resetting healthy viewers or replacing
the shared capture worker. The publisher retains the same eight member slots,
physical pool and source-owned IDR coalescer. A recovering original member, unlike
a provisional late join, can keep its source alive after other viewers depart.

The network owner calls `Subscriber::dispatch_recovery` before ordinary control
dispatch. A positively negotiated, full-view failure request fences that viewer's
old media immediately and charges its existing sender recovery allowance. One
absolute deadline covers fresh attachments, waiting for the next admitted source
IDR, configuration and first decode; duplicates and healthy source progress cannot
extend it. A native capture already in flight still completes for healthy viewers,
not the failed cache.

`recovery_state` exposes whether three fresh local attachment tickets are needed.
Generate these outside publisher locks, then drive `advance_recovery` between
original-session renewal turns. `owns_recovery_record` keeps attachment records
with their original transport owner. After attachment, ordinary publisher capture
and `Subscriber::service` drive the existing shared decoder handshake. The original
source coalesces recovery and late-join IDR demand under its existing 500 ms rate
allowance. The sender is rebound, not reconstructed; old failure and repair spending
is retained. No media goes to the recovering viewer before Configured, and no
readiness is reported before its matching FirstDecoded.

Source consent and each viewer's observation authority remain independent. Failure,
timeout, cancellation and invalid replacement affect only that subscriber unless
it was the last source owner. A decode report never grants visibility or input.
Control-intent sessions do not use this observation-only path. Existing transport
namespace exhaustion still refuses rather than recycling retired bindings.

## Executed checks

Seven new tests cover real lost datagrams through fresh attachments and native
worker IPC, a healthy peer retaining its generation, recovery by the sole remaining
subscriber, failure while capture is already in flight, immutable deadlines,
foreign-connection refusal and invalid ticket isolation. The encoder/decoder child
payloads are explicit fixtures, not hardware HEVC qualification. All 46 tests in
the complete shared-startup target pass. Production daemon and full shared-startup
test-source strict pedantic Clippy, changed-file formatting and whitespace pass.

All eight relevant first-party libraries were rebuilt from the checksum-verified
7ee3f649 source plus these changes with nightly-2026-08-31. External libraries came
from checksum-verified GitHub run 35461102442 (ea73e284); every external lockfile
package version/checksum and the exact compiler match. The cold Cargo daemon build
was killed at Asupersync code generation by the local memory limit. These results
are not a cold-dependency, full-workspace, GPU, physical-presentation, live-tailnet
or independent-transport qualification claim.

## Canonical session integration

`SharedHost::drive` and `serve` now admit recovery before ordinary media service,
advance the original subscriber's replacement, and complete its decoder handshake
inside the existing observation-renewal and UDP loop. There is no manual host
handoff or second network owner. The existing fresh nonce supplier supplies three
one-use tickets, outside publisher locks, with source and viewer authority checks
before and after each call. Failure retires that viewer; source revocation fences
all viewers before another write. The same cancellation guard covers an unpolled
recovery turn.

The failed generation's presentation verifier and pending metrics requester are
retired immediately. Attachment records stay in their original transport slots;
renewal dispatch validates duplicate recovery requests against the previous full
binding and consumes obsolete advisory reports without granting credit, readiness
or another deadline. After matching FirstDecoded, the host installs fresh
presentation and metrics scopes, updates the repair route, and preserves cumulative
report counters. `SharedStatistics` exposes recovery_requests and recovered_streams
as protocol milestones, not evidence of visibility. A new decode cannot restore an
old visibility claim or grant input. A bounded immutable selection snapshot is kept
once per SharedHost, not allocated on every network turn.

Seven further canonical-session tests cover automatic replacement and continuing
healthy delivery, duplicate and malformed requests during attachment, entropy
failure, cancellation, revocation inside ticket generation, and actual negotiated
presentation/metrics exchanges across generations. The latter proves that old
visibility is withdrawn, old reports during recovery cannot restore it, new scopes
accept fresh sequence-1 reports, totals survive, and observation remains distinct
from input authority. These tests use real TLS/UDP and original session owners;
the loss report, visibility, source payload and decoder acknowledgements are
explicit protocol fixtures. Native decoder reuse remains exercised by the separate
shared-startup tests above.

Final canonical validation includes 23 passing original-session/shared-session
unit tests (seven new, sixteen unchanged), all 46 shared-startup integration tests,
ten unchanged shared-late-join integration tests, and full daemon test-source
strict pedantic Clippy. The focused runtime harness
changes only test registrations in an external source copy; all production code,
original test bodies, assertions and deadlines are retained. The entire Linux daemon unit-test
target is type-checked and linted; only the stated subset is executed. Eight relevant first-party
libraries were rebuilt from the checksum-verified e815b017 archive plus this slice,
with the same exact external compiler/lockfile inputs described above. This is not
a full Cargo/workspace or hardware/live-tailnet qualification claim.

This does not close the broader viewer admission or loss-recovery beads. Refs:
plan 7, 11, 12.3 and 17; `fr-p2-viewer-admission-e62` and
`fr-p1-loss-recovery-20s`.
