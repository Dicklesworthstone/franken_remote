# Native input ticket renewal

Input-validity tickets now travel from the canonical native input owner over the
existing authenticated QUIC input-feedback stream to the viewer. A ticket renews
only the short-lived credential for future actions. It does not create or renew
a control lease, observation authorization, input mapping, source observation,
or visible-frame evidence.

## Preserve in-flight input

`SessionAuthority` retains at most eight live tickets for its current control
lease. Each retains its original exclusive expiry, capped by observation and
control authorization. A new ticket does not invalidate an earlier in-flight
action. Full capacity refuses issuance rather than evicting a live credential;
expired entries can be reused. Zero and duplicate retained ticket IDs refuse.
The caller remains responsible for unpredictable, never-reused credential IDs.

Stale view, suspension, revocation and input-mode changes invalidate **all**
retained tickets, not just the latest ID. An input-mode change requires a newly
issued ticket. No ticket transition resets consumed action or pointer sequences,
restores failed actions, changes native receipt contents, or permits retry of an
unknown external effect.

## Wire representation

`InputTicket` (`0x0017`) is 128 bytes including the existing 24-byte FRD0 header.
The 104-byte body is, in order: remote session, input lease and ticket IDs
(16 bytes each); geometry, viewport, codec-configuration and recovery generations
(8 bytes each); issuance sequence, host-monotonic issue time, host-monotonic
exclusive expiry (8 bytes each). Integers are big-endian. Zero clock samples,
sequences and initial generations are valid; zero IDs or channel bindings are
not. The transmitted lifetime must be positive and at most 1,500,000 microseconds.
Trailing data, invalid mandatory extensions, wrong direction and datagram
carriage refuse; the negotiated control-message ceiling remains enforced.

The route must be explicitly installed as `Messages::InputFeedback` on both
endpoints. It admits only `InputTicket` and `InputResult` (`0x0048`) on one
host-initiated, reliable, critical-priority stream, with the same binding as the
ordered input stream. Legacy exact-result routes remain supported but refuse
renewal. Route installation is an authenticated session decision; this code does
not automatically advertise a capability or infer peer support from traffic.

## Host ownership and transport

`QuicInput::renew_ticket` queues issuance in the existing single-command native
mailbox. The native owner checks current cancellation/admission and uses its
retained runtime clock when it actually issues the ticket, not a peer timestamp
or an earlier enqueue timestamp. The session must call renewal on idle turns,
after servicing ordered input, and drive the existing watchdog independently.
The API starts no thread, timer, task, listener or replacement executor.

There is at most one command or one fixed-size unsent feedback record. An action
receipt has priority over renewal and cannot be overwritten or relabelled as a
ticket. The supplied credential generator is not called while that slot is busy
or before the 250-millisecond issuance cadence. Actual ticket bytes, sequence and
native expiry survive transport backpressure unchanged. A failed or abandoned
send never issues a replacement or re-encodes old input. An unsent expired ticket
terminates the input attachment rather than gaining another transmission window.

Unlike a receipt for an already committed effect, a ticket cannot be sent after
input revocation. Existing connection-identity, stream FIN/RESET and cancellation
guards apply, including when an unpolled drive future is dropped. Native cleanup
still runs independently, and handoff still waits for its confirmed completion.

## Viewer deadlines

`InputClient::accept_ticket` checks exact session, lease, view, monotonic issuance
sequence and host-clock evidence. It accepts an initial matching credential once
so a bootstrap grant can acquire its actual deadline; replay cannot retime it.
`PresentedInput::accept_ticket` also requires the correlation's host boot to match
the media tracker. Clock evidence must come from this authenticated session's
existing clock exchange, not an arbitrary caller-supplied remote timestamp.

`ClockCorrelation::deadline_lower_us` derives a conservative local expiry from
the full measured exchange interval and the qualified relative clock-drift bound.
It also caps validity by the correlation's own deadline. Network delay, queued
send time and delayed callbacks are charged to the credential's lifetime; receipt
time never starts a fresh duration. Expired/regressing/overflowing correlations
are refused, not saturated into a longer authorization.

Ticket expiry alone pauses new actions and pointer updates without consuming
identities or deleting pending receipts. A genuinely newer valid ticket can
resume only an otherwise-live client with fresh presentation/mapping evidence.
Hidden, unfocused, suspended, disconnected or otherwise stopped clients never
reopen. After entering this network-bounded path, the legacy opaque-ID setter
cannot bypass it. Late input receipts retain their original meaning.

## Verification and scope

Seven core regressions cover overlapping in-flight credentials, bounded capacity,
original expiry, authority caps, lifecycle boundaries and complete mode fencing.
The core increment published as `46b7f70` reused the exact source objects from
successful full-workspace `fast` and documentation verification in GitHub run
34424299214, attempt 2. Attempt 1 exited in the pre-existing X11 input suite
without an assertion result; the unchanged rerun passed. No source, expectation
or timeout was relaxed for that rerun.

Three independent wire tests cover byte-exact layout, truncation, limits, roles,
invalid values and diagnostic redaction. Six client tests cover delayed and late
arrival, replay, foreign generations/boot, bootstrap deadline binding, pending
receipt retention and lifecycle refusal. Two clock tests cover different origins,
drift, exclusive expiry and arithmetic boundaries. Seven native integrations use
actual localhost UDP/TLS and X11/XKB/XTest for in-flight input across rollover,
input beyond initial ticket expiry, send backpressure, original expiry, receipt
priority, abandoned I/O and exact route families. Native integration grants,
visibility and shared-runtime clock evidence are explicit test fixtures, not
live tailnet approval or physical-display measurement.

```sh
cargo test -p fr-core --test ticket_renewal --locked
cargo test -p fr-wire -p fr-client --test input_ticket --locked
cargo test -p fr-media --test clock_deadline --locked
cargo test -p fr-native --all-features --test input_quic --locked -- --test-threads=4
./scripts/verify.sh fast
./scripts/verify.sh docs
```

Local runtime verification rebuilds first-party crates against matching retained
Asupersync TLS dependencies and the pinned compiler, rather than freshly building
every external dependency. The full-workspace candidate verification is recorded
separately in the publication commit. No Asupersync dependency/release change is
needed by this implementation. Controller-specific lease challenge coordination,
platform input sampling and the complete desktop application loop remain separate
integration work; this feature does not close those broader qualification gates.
