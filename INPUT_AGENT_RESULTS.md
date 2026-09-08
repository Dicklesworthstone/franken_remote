# Native input results and final cancellation

This slice extends the existing `frd::input_agent::{Seat, Agent, Driver}` and
`frd::input_watchdog::Watchdog`; it introduces no competing native owner,
runtime, transport, codec, dependency, or public listener. Its upstream source
baseline is `38c5bea7313e6948d9c7f30ebfbfdc02a842007d`.

## Final submission includes runtime cancellation

The native loop's cancellation check is not sufficient: the parent context may
be cancelled during platform preparation, before the independently scheduled
watchdog runs again. The clock callback passed into `InputSession::dispatch`
now checks `Cx` cancellation after **every** native preparation, fences the
shared input control, and then samples the host clock. Final policy admission
therefore rejects the next operation even during watchdog scheduling lag.
Authority mutations also check cancellation immediately before attempting the
mutation. This does not undo an already entered native call. The confirmed
prefix of partially submitted text remains in its receipt.

Two deterministic regressions deliberately leave the watchdog owned but not
yet scheduled while preparation cancels its context. Both fail against the
unchanged `3f4f39b` input-owner source (also unchanged in `38c5bea`): it submits the key or the second text scalar. Both pass
with the final-submission check. A separate real-XKB regression verifies that
cancelled preparation does not press the key or strand its repeat setting.

## Return the original action's result

`Agent::try_input_result` and `Agent::input_response` project the existing
native `Reply` into `InputReply`, using `fr_wire::input_result::InputResult`.
The existing raw `try_reply` and `response` APIs remain available.

The original validated request's channel, remote session, input lease, and
**action versus pointer sequence space** are captured only after enqueue
succeeds. These values are never reconstructed from the current controller.
A rejected second enqueue cannot replace the uncollected result's context.
A late result can still be collected after cleanup and controller handoff,
with the original binding. No ticket, key identity, coordinate, or typed text
is added to the result.

Only actual retained receipts produce `InputReply::Record`. The existing
codec determines admitted/submitted stage and unknown-next-operation semantics;
this path never manufactures observed application effects. A reliable native
panic's retained receipt preserves its confirmed prefix. Evicted receipts,
obsolete pointer records, refusals before receipt admission, cancelled-before-
start commands, initialization failures, and panics without a receipt remain
explicit non-record dispositions rather than fabricated zero-effect success.

Selecting an input-result API for an authority command returns
`NotInputCommand` without consuming its reply. Dropping an input-result wait
has the same behavior as dropping the existing raw response wait: input is
revoked, but the actual result and its binding remain collectable. This is not
permission to replay a timed-out action.

## Native execution and evidence

Four additional integration tests compose actual FRD0 input bytes, the canonical
X11 factory/native owner and independent Asupersync watchdog, XKB/XTest, and the
host-to-viewer result codec. They live in `input_agent_result_x11.rs`, alongside
the existing `input_agent_x11.rs` tests; the factory and prior tests are preserved. Each test creates private local Xvfb servers; it does not use
the user's desktop. They verify:

- Lease expiry without further traffic releases a real key, modifier and drag,
  restores key repeat, and permits handoff only after native cleanup.
- Local revoke, view invalidation, suspend and disconnect release a real drag
  without another input packet.
- A blocked real XKB preparation expires independently. The driver's bounded
  drain reports unresolved cleanup and keeps the seat occupied. Once native
  preparation resumes, no key is pressed, the preparation is restored, and
  the native finalizer publishes cleanup completion.
- Parent cancellation inside real XKB preparation rejects the pending key
  even before the watchdog is scheduled, and restores native preparation.

Reproduce on a normally provisioned checkout with the pinned toolchain:

```sh
cargo test -p frd --test input_agent_cancellation --test input_agent_results --locked
cargo test -p fr-native --features linux-input-agent --test input_agent_result_x11 --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
./scripts/verify.sh fast
./scripts/verify.sh docs
```

The local development verification used Cargo for 177 core/wire/media tests
and directly rebuilt first-party runtime/native sources with the pinned
compiler and matching, previously retained Asupersync libraries. It passed
236 selected tests: those 177, 12 existing native-owner tests, nine watchdog
tests, seven worker/media-authority tests, 14 prior X11 tests/unit tests,
two cancellation regressions, six result-projection regressions, and four
new native integrations, and five existing X11 factory tests. Strict first-party/test Clippy, formatting, docs,
and three repeated parallel runs of the owner/new test binaries passed.
This is **not** a fresh full Cargo-workspace rebuild or a new remote CI run.

Local authority grants in these tests are explicit test inputs, not evidence
of Tailscale identity admission, user consent, transport authentication,
physical-compositor support, or a shipping unattended desktop. Xlib/process
failure may still leave release uncertain; threads are not process isolation.
The containing share-session owner must poll `Driver` independently, maintain
one shared seat, wire real OS lifecycle signals, and retain the existing
quarantine behavior when native cleanup cannot be confirmed.

These changes address portions of `fr-p1-input-pipeline-ay1` and
`fr-p1-session-agent-iq3`; their broader acceptance criteria are not closed.
