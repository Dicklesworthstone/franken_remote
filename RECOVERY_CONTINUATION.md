# Automatic observation-viewer recovery

The running `StreamingViewer::serve` loop now continues an observation-only
session from an actual reference failure through fresh media attachment, the
existing native decoder handshake, and resumed video. The original connection,
receiver reservation, decoder process and local stop handle remain owned by the
same session. It neither reconnects nor grants control.

## One failure, one original deadline

This path requires positive `reference-recovery` version 1 negotiation and the
original receiver reporter in `Requested` state. An admitted send is not proof
that the host consumed the request. The viewer therefore keeps servicing its
original session and waits for the exact next-generation configuration binding
on the original control stream before resetting old media. Resetting sooner can
race ahead of the reliable request and terminate the host's still-active stream.

The original local failure deadline covers that wait, all three one-use
attachments, the configuration record, native first decode, and both decoder
acknowledgements. Observation renewal continues throughout, but cannot extend the
recovery deadline. Cancellation, expiry, malformed offers and failed handoff
close the parent and fence the receiver before native cleanup. A control-owning
or control-requesting viewer retains its separate terminal/reacquisition rules.

The existing `Replacement` owner, not application callbacks, consumes attachment
records. One bounded buffer holds the new configuration. `ViewerRecovery` checks
the original sent report and decoder/receiver identity before admitting it;
changed canonical parameter sets, dimensions or frame rate are not accepted as
a same-configuration recovery. The existing decoder process supplies both API
configuration state and the genuine first-decode completion. The network keeps
moving during native work, and a polled session turn is completed rather than
dropped when decoding wins the race.

## Fresh evidence after handoff

Old media and pending repairs cannot reach the new receiver generation. Old
solicited metric queries are validated and retired rather than answered with
measurements from the replacement receiver. Queries for the admitted new view
stay queued for its own feedback owner. New feedback and presentation owners
start with unknown evidence; only lifetime report counters are carried forward.
No old frame is relabeled fresh. `Statistics::recovered_streams` counts completed
attachment/decoder handshakes, not visible pixels or input grants.

The recovered frame is delivered through the ordinary presentation callback,
then dependent frames continue in the ordinary receive/decode loop. A poisoned
or uncertain in-flight native decoder remains terminal; this path does not reuse
such a process or synthesize a successful decode.

## Verification scope

The continuation tests exercise `StreamingViewer::serve` itself against a real
TLS/UDP peer and a real supervised child process using explicitly synthetic media
payloads. They cover recovery and subsequent dependent frames on the same native
process, a host delayed beyond the initial observation lease, a missing offer,
a changed native configuration, and local stop during the recovery wait. The
peer services real selective-repair requests before abandoning the reference
chain; leaving those old reliable records blocked is not a valid renewal test.
These are protocol/ownership/IPC checks, not native HEVC or hardware qualification.

## Remaining integration

The host peer in these tests uses the production packetizer and replacement
handshake, but its orchestration is test-owned. The running host must still join
accepted requests, the original capture owner and `recover_sender` automatically.
Do not globally advertise complete automatic desktop healing until that host
join and real-HEVC injected-loss qualification pass. This observation-only change
does not implement automatic control reacquisition or replay old input.

Owning design: plan sections 7, 11, 12, 17 and 19; recovery work tracked under
`fr-p1-loss-recovery-20s`. No protocol limit, authority lifetime, dependency pin,
shipping runtime, or codec has changed.

### Retained local checks

Five continuation tests pass on the focused run and in the broader daemon run.
The latter finished with 312 passed, two failed and 16 explicitly ignored. The
file-offer expiry assertion also fails on the pre-change recovery baseline. The
old recovery-report test hit a `Backpressure` unwrap in the broad run and passed
when rerun alone; the original failure is retained, not declared fixed. No
assertion, ignored-test flag, or production deadline was relaxed.

The core/wire/media/client Cargo suite passes 696 tests including doctests, with
all features and the locked offline dependency sources. Strict Clippy passes for
that Cargo scope and for the rebuilt daemon library and complete daemon test
source. Changed-file formatting, whitespace and documentation checks pass.

Source inputs are the checksum-verified ba79119 archive plus the current 1cf04
recovery implementation restored from repository blobs. First-party libraries
are rebuilt; unchanged upstream native libraries come from the compiler-matched
CI 35421807938 artifacts. The five modified pre-existing files match their
1cf04 base blob hashes. The patch does not include synthetic baseline restoration
commits or concurrent file-scope changes. This is not a clean full-workspace
Cargo build or full-current-main qualification.
