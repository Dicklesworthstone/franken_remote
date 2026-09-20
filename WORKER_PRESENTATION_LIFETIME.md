# Crash-safe worker presentation

A decoder attached to a local UI target now renders into one fixed-size child
window owned by its original X11 connection. The original UI window remains the
input/viewport target and is never adopted or destroyed by the decoder. Remote
pixels are submitted only to the child. The parent has a black background;
closing the renderer connection removes its child and exposes that background.
This works for orderly Stop, parent-pipe EOF, typed worker refusal, and SIGKILL,
which cannot execute a Rust/C destructor or cooperative cleanup callback.

The existing single retained XImage and `maintain_presentation()` implementation
are preserved. No pixmap/history queue, extra decode, source observation, frame
identity, input grant, or visibility acknowledgement is introduced. The extra
server resource is one geometry-bounded child window per attached renderer;
server/compositor backing memory is not claimed to equal the reported XImage
bytes. The existing count/byte/geometry bounds continue to bound pixel storage.

Both the original parent and child are checked before presentation and idle
repair, sharing the original 128-event turn bound. Resize-away-and-back,
unmap/remap, reparenting, child movement, or destruction permanently retires the
attachment. Retirement releases the retained image and closes only its private
presentation connection. Capture connections may own borrowed DAMAGE resources
and are not closed by this presentation-specific path. Dropping a retired or old
attachment cannot erase a newer renderer's child or revive an old view mapping.
The child selects no input events, changes no focus, and makes no input grab.

## Verification and limitations

The unchanged regression `hard_kill_clears_original_target_without_cooperative_stop`
fails against bridge blob `47ca813dd349ea3787da91077f980d24252d14ca` from
`17f95e7d768d06b974f742897655978c6a2e360b`: after a real HEVC decoder is killed by
SIGKILL, independent X11 readback finds non-black pixels still in the UI target.
The repaired implementation passes the same regression.

The initial, pre-reconciliation focused run passed 26 tests: 16 new native/worker tests, nine existing
idle-presentation tests, and the existing fitted-presentation test. All nine
upstream idle test bodies remain byte-identical (blob
`8b42011226305eea85b2d902a4808b4ceec0ad1f`). Their independent exposure fixture now
resolves the actual child drawable before clearing it, so its pre-repair pixel
loss and exact-restoration assertions remain meaningful. Parent lifecycle
helpers still target the original parent. The initial fixture mismatch is
retained as a failure, not represented as a passing unmodified harness.

New tests exercise actual Xvfb/X11, software HEVC encoding/decoding in a distinct
production worker, fitted output, dependent reference-only decoding, orderly
Stop/EOF/refusal, hard kill, old-versus-new attachment cleanup, and terminal
mapping changes. An XTest fixture confirms original parent focus, exact pointer
coordinates, propagated key/button events and no parent visibility-loss event
from the child. This is X-server routing evidence, not physical input or a
FrankenRemote control-authority qualification.

Execution uses pinned nightly-2026-08-31, Debian FFmpeg 7.1.5, archive source
`1397d5074582827570b92f32c2b8809ee192ddc2` for the four rebuilt first-party Rust
libraries/worker, plus the current bridge and exact current presentation module
from `17f95e7d768d06b974f742897655978c6a2e360b`, then this patch. A separate Cargo
harness runs the exact test files without the native package's unrelated async
dev-dependencies; no alternate/older runtime or mock codec is linked. Production
linux-media/linux-displays library and worker builds, strict production Clippy,
strict pedantic test Clippy and formatting passed. This is not a complete current
main/workspace run, hardware/GPU/compositor/physical scanout or live-tailnet proof.

The X server, OS and selected desktop user remain trust boundaries; another
client may have retained pixels previously delivered to it. The change removes
this worker's drawable on connection death, not every possible copy of pixels.
Authority revocation and hung-worker supervision remain the existing separate
owners. No release gate or bead is closed by this scoped implementation.

## Reconciliation with concurrent idle-worker integration

Publication was reconciled against `7ee3f649d29c4f28e0cc15dfb33ff24759f3de54`,
not applied over the older presentation files. The original unbuffered command
reader, native readiness waiter, parent-command/EOF priority and idle worker loop
are preserved. The bridge consumes parent, child and unrelated events under the
same original 128-event total bound; a ClientMessage cannot cause a busy loop.
The shared exposure fixture retains upstream noise-event injection and resolves
only pixel-clear operations to the actual content drawable. All twelve current
`idle_presentation` test bodies remain byte-identical to upstream.

The reconciled focused run passed **43 distinct tests**: sixteen canvas/worker,
twelve current idle-presentation, one fitted-presentation, seven DAMAGE-capture
and seven selected-monitor DAMAGE tests. Both presentation-only and displays
feature configurations built and ran. Strict pedantic Clippy for the focused
presentation tests and production worker, native-library pedantic checks, and
changed-source rustfmt checks passed. The identical final SIGKILL regression
also fails against current upstream bridge
`55856b53e5e6e4ae3494a39f8d47ba72af4d5b41`, with the current idle worker, because
remote pixels remain after process death; it passes against the reconciled fix.

The first combined run retained one expected integration mismatch: the new
canvas-resize test sent a follow-up frame expecting a Refused reply, but the
concurrent idle worker had already terminated with GeometryChanged. The test
now requires EOF within the original three-second response deadline, exit code
2, child removal and black pixels **without sending another frame**. It does not
accept a fabricated unsolicited reply, relax a timeout, or change production
worker behavior. The independent SIGKILL test is unchanged.

This rerun uses checksum-verified `baab9c628` source for the four first-party
libraries, the exact published native presentation/worker changes through
`7ee3f649`, and this reconciled slice, with the same pinned compiler and native
SDK. Small external manifests select the existing native feature dependency
graph and point directly at those production sources and exact test files;
repository manifests and dependencies are not edited. No retained external
runtime, mock codec or substitute implementation is linked. Unrelated later
media-pipeline/cursor/session changes, the complete current workspace and the
broader async integration suite remain outside this executed scope. Existing
`idle_worker` test source and the production worker are preserved, not claimed
as an additionally executed ten-test target in this rerun.
