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

Automatic SharedHost dispatch and generation-specific feedback/presentation
handoff are the next integration step. This does not close the broader viewer
admission or loss-recovery beads. Refs: plan 7, 11, 12.3 and 17;
`fr-p2-viewer-admission-e62` and `fr-p1-loss-recovery-20s`.
