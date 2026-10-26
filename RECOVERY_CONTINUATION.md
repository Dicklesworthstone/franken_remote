# Automatic viewer recovery

The running `StreamingViewer::serve` loop continues an observation session, or
since 2026-09-30 a granted control session, from an actual reference failure.
It goes through fresh media attachment, the existing native decoder handshake,
and resumed video. The original connection, receiver reservation, decoder
process and local stop handle remain owned by the same session. It neither
reconnects nor grants control.

For a control grant (plan 11.3, 12.3):
- `take_media_for_recovery` suspends the grant's input at once, because the
  lost reference means the view cannot advance. The one never-admitted record
  is abandoned.
- The recovery drives the grant's own turn, so renewals, tickets, results and
  the clock stay serviced while it holds the media. An interactive embedding
  keeps seeing its grant, not an observer.
- `install_recovered_media` moves the suspended grant to the recovered
  receiver's generation (`PresentedInput::follow_recovery`). Input resumes on
  that view's fresh evidence plus a ticket naming it, as after any suspension.
- A grant that is still being acquired keeps the terminal refusal.

## Interactive viewing before a control request

The control-capable `serve_interactive_control` and
`serve_interactive_control_until` paths can retain `InteractiveState::Viewing`
through the same negotiated recovery. This covers an embedding that continues
watching before its explicit local decision to request control. It does not
change the native CLI's separate focus wait after a grant.

During that media gap, the original observation renewer, clock, input attachment,
decoder process and receiver remain owned by the same session. No control request
or request deadline is created, no native input seat is reserved, and no grant
is implied. Mapping, visibility, evidence and request callbacks against the
failed generation refuse. The UI keeps its `Viewing` state with no presentation
until the recovery handshake completes.

The completed media owner must be the exact reference-only successor on the
original connection and receiver lineage. Its new configuration attachment and
recovery generation update the selected target; parent, request sequence,
display geometry, codec configuration, viewport, bounds and requested capabilities
stay fixed. The existing physical coordinate-map confirmation survives those
unchanged dimensions. Old presentation and source evidence do not: the real
recovered decode receipt still needs an independent visibility callback. A later
explicit user decision starts the ordinary request deadline against the current
target. Once that decision has staged a request, recovery cannot rewrite it or
restart its deadline; reference loss remains terminal for that pending request.

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
close the parent and fence the receiver before native cleanup. A viewer still
acquiring control retains its separate terminal/reacquisition rules.

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

The 2026-10-26 pre-request-viewing regressions are in
`crates/frd/src/session_startup/running/streaming/recovery/tests/viewing.rs`.
They use both canonical loops, actual TLS/UDP and supervised media subprocesses,
with explicitly synthetic codec, visibility, consent and OS-input fixtures. They
check reference loss before a request, no implicit native owner, old-generation
callback refusal, preservation of the confirmed map, a later current-generation
grant and input receipts, local stop during recovery, and terminal failure once
a request was explicitly staged. Source formatting and whitespace checks have
passed. These new tests have **not been compiled or executed in this session**:
the shared build filesystem and elevated temporary target mount exhausted their
available capacity. The historical results below do not validate this change.

```bash
cargo test -p frd --lib session_startup::running::streaming::recovery::tests::viewing --locked -- --test-threads=1
```

## Remaining integration

The original viewer-only tests retain a manually orchestrated peer. The running
host is now joined separately in `RECOVERY_HOST.md`, including an integration
test that runs both canonical peers through loss, fresh attachments, native IPC
and resumed dependent frames. Real-HEVC injected-loss qualification still remains
open; these tests do not globally qualify automatic desktop healing. A
control grant's recovery keeps its suspended lease; it never reacquires
control or replays old input.

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
