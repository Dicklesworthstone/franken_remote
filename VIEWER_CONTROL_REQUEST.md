# Requesting control on an existing viewer session

The viewer now has a network-backed counterpart to the existing
[host grant broker](BROKER_CONTROL_GRANT.md). It sends the real control request
on the admitted connection and consumes the host's initialized grant; callers
no longer have to manually interleave this exchange with observation renewal.

## API and ownership

`ViewerSession::request_control(&channels, &mut clock, request, policy).await`
uses an explicitly owned `ClockSync` already bound to this session.
`ViewerSession::request_control_synchronized(&channels, request, policy).await`
uses the exchange previously enabled through `enable_clock_sync`. It returns
that same endpoint to the session after success, preserving its measurement and
outstanding probe. It never creates or enables a clock implicitly. A caller with
a session-owned clock must use the synchronized path, not a competing endpoint.

Both return `Result<(fr_wire::control::Granted, InputClient), ControlledViewerError>`.
The request must match the original control binding, attached input channel,
display target and selected limits. RequestControl role and the grant capability
must already have been negotiated. A usable measured clock sample precedes send.
The initial input ticket is the one received from the actual host broker, not a
locally manufactured credential.

The request owns one bounded record. Its existing two-second deadline starts
when the future is constructed, including time before first polling, clock
measurement, local approval, native initialization and send backpressure.
Backpressure preserves the original bytes and deadline. Timeout, refusal or
cancellation never retries the request under a new identity. Dropping even an
unpolled future closes the original session and stops the request's clock.

During the exchange the original observation responder and clock continue
progressing. Unrelated media/configuration records remain in bounded transport
queues; this method does not decode or present them. On the host, the existing
GrantBroker and native Driver must remain serviced with genuine local consent,
readiness and qualified credential sources. This viewer API does not provide
those host prerequisites or extend any host authorization deadline.

## Handoff to controlled viewing

Receiving a grant does not make old pixels trustworthy or confirm coordinates.
Join the returned InputClient to the actual ReceivePipeline using PresentedInput,
confirm the actual mapping, and supply genuine decoder/presentation evidence.
Then use `into_controlled` with the original external clock, or
`into_controlled_synchronized` with the retained session-owned clock. Existing
controlled viewing continues to check ticket expiry, view freshness and exact
receiver identity before emitting input. The native owner still checks authority
immediately before OS submission.

This implements the initial network grant exchange, not an installable desktop
application or automatic promotion of every StreamingViewer. The remaining
application handoff must keep the actual decoder/receiver and qualified platform
visibility intact; it must not fabricate a startup frame to enter control.

## Verification scope

Ten integration cases exercise production UDP/TLS startup, negotiated channels,
clock exchange, the host grant broker and the native authority owner. They cover
immediate and delayed approval, binding refusal, missing clock, duplicate clock
ownership, unpolled cancellation and construction-time expiry on both clock paths.
The accepted grant still refuses a key action with MappingUnconfirmed, and Seat
handoff follows actual native-owner cleanup. Local consent/readiness and the
counted native sink are explicitly test fixtures, not live Tailscale or hardware.

Reproduction on the repository-pinned toolchain with native build prerequisites:

```bash
cargo test -p frd --lib session_startup::viewer::controlled::request --locked -- --test-threads=1
cargo test -p frd --lib session_startup::viewer::controlled::request --locked -- --test-threads=8
./scripts/verify.sh fast
./scripts/verify.sh docs
```

Local first-party rebuilds against verified, compiler-matched Asupersync 0.5.0
artifacts passed the ten cases and a 154-test daemon selection. Sixteen existing
native/namespace tests in that selection remained ignored. Strict daemon test
Clippy and changed-source rustfmt passed. The local source selection includes the
current viewer/clock owners but is not a clean full-current-workspace rebuild;
committed-source CI is a separate gate. A local cold dependency build was killed
at the 4-GiB memory limit while compiling Asupersync. No physical-input, hardware
video, independent-peer interoperability or complete-app gate is claimed here.
The broader fr-p1-input-pipeline-ay1 and fr-p1-fr-client-bis acceptance remains open.
