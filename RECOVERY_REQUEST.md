# Bounded reference recovery

The delivery layer implements `RecoveryRequest` (`0x0036`, protocol section 6),
per-subscription sender fencing and chronic-failure refusal, and one fixed-space
shared-encoder IDR admission queue. Native subscriptions also join accepted
requests and terminal sender failures to their actual input authority and capture
worker. This is not yet automatic control-stream routing, channel reattachment
or hardware qualification. The application must negotiate `reference-recovery`
version 1, route requests through the original admitted control connection, and
perform the existing fresh-binding and decoder recovery handshake.

## Receiver and control record

Construct `RecoveryRequestor` beside the real `ReceivePipeline` after admission,
using the installed full decoder binding. Feed only actual successful decode
receipts to `observe_decoded`; absent evidence is unknown, not frame zero. Poll
`offer` even without packets. It ticks the real receiver and emits a request only
for reference expiry, recovery expiry, or reported decoder failure. Cancellation,
malformed input and clock failure remain terminal.

A request contains the existing 96-byte full view binding, followed by a one-byte
version (1), reason (1 reference expiry, 2 recovery expiry, 3 decoder failure),
known-frame flag (0/1), and big-endian u64 frame number. Unknown must encode zero;
known frame zero is distinct. Including the ordinary 24-byte FRD0 header, the
extension-free record is 131 bytes. It travels reliably viewer-to-host on the
parent control binding. Direction, binding, version, lengths and canonical values
are checked without payload allocation; header extension rules are unchanged.

Retain an offered record through backpressure, authorize immediately before its
actual write, and call `mark_sent` only after the bounded transport accepts it.
There is one request and one original deadline per failed receiver generation.
Small output buffers, polling and duplicate offers cannot renew that deadline.
A sent request still times out while waiting for replacement. Decoder-owned
pictures remain charged until their actual owner releases them. Construct a new
requestor only after an admitted receiver replacement; old scope proofs cannot
follow it.

## Host and shared encoder

Pass the installed binding, not a peer-proposed tuple, to
`SendCache::request_recovery` on the original subscription. Invalid requests and
claims about future frames leave the healthy cache usable. Acceptance clears old
originals, repairs and observations and invalidates prepared packet offers before
returning one non-cloneable `RecoveryDemand`. Duplicates coalesce without extending
the original deadline or issuing another demand. Expiry closes this sender;
replacement at the deadline cannot resurrect it. A valid replacement requires a
new generation and newer channel bindings under the existing rules.

Keep one `IdrCoalescer` for the shared encoder lifetime, not per viewer or
recovery generation. Its sole slot consumes authentic sender demands and keeps
the earliest live cohort deadline. A valid newly arriving demand retires an
already expired cohort before joining the queue, even when the idle source has
not polled `take` yet. The new viewer does not inherit the old viewer's expired
deadline; the old subscription still expires independently. Invalid demands
cannot discard another cohort's work.

The configured 100 ms to 1 s minimum interval bounds enqueue rate, including
failed enqueue attempts. New generations, cancellation and expired-cohort
retirement do not replenish the rate allowance. A different viewer's request
never resets healthy bindings or decoder state. The actual native owner must
check authority, fence stale-view input, and enqueue force-IDR work within the
returned deadline; this delivery API is not itself an input grant or an encoder.

## Chronic-viewer refusal

`SendPolicy::max_recoveries_per_window` and `recovery_window_micros` default to
four accepted failed generations in a sliding ten-second window. Configuration
is bounded to 1..=16 recoveries and 1..=60 seconds. Fixed timestamp storage belongs
to the subscription, not its replaceable media generation; there is no growing
request history or per-request allocation.

A validated recovery request spends one admission BEFORE a `RecoveryDemand` can
reach the shared encoder. Generation replacement does not charge that same
failure again. Duplicate requests, dropped demands, payload eviction, recovery
and codec-generation changes do not refund credit or move its original timestamp.
Invalid requests/replacements spend nothing. Replacing an already expired unsent
original also consumes an admission even when the owner has not called `tick`.
Healthy reconfiguration does not consume or replenish recovery credit.

Exhaustion returns `SendError::RecoveryLimitExceeded` and closes only that sender,
clearing cached payloads and invalidating prepared packet offers. Later timestamps
and fresh channel bindings cannot revive the closed cache. Do not automatically
construct replacement subscriptions to bypass this refusal. An independently
admitted lower operating point remains a separate application decision; this
policy does not silently change the shared encoder or another viewer's quality.

## Native authority and capture

`Subscription::request_recovery(source, bytes, installed_binding)` joins an
accepted request to the actual `CaptureSource` which supplied that subscription.
A foreign worker with equal frame/configuration IDs cannot receive the demand.
The installed session identity is checked under the original shared authority
mutex. After bounded request admission, view readiness and old input tickets are
invalidated under that same mutex, before any native work; malformed requests
leave a healthy view alone. No authority lock is held across IPC or an await.

Terminal sender refusal also invalidates readiness and old input tickets before
returning from that mutex, even though no encoder demand is issued. This includes
chronic-failure exhaustion, closed senders, recovery expiry and sender clock
failure. Restoring readiness alone cannot revive an invalidated ticket. Refusal
does not cancel another viewer's queued IDR work or revoke observation authority.

Each source owns one `IdrCoalescer` with a 500 ms interval for its entire lifetime.
The existing `capture` / `capture_if_changed` loop consumes due work and sets the
real worker force-IDR flag. The original recovery deadline caps the entire IPC
operation, including repeated NeedInput / Poll replies and time spent waiting
before capture. A worker returning a dependent picture despite force-IDR is
refused and poisoned, not presented as a recovery frame. An expired cohort is
discarded without resetting healthy capture; failed subscriptions still expire
independently. Unrestricted worker access abandons queued work without refilling
rate credit. Idle source owners must wake at `next_recovery_deadline`.

Successful capture does not make the view ready, renew a ticket, or install a
channel. Fresh binding admission, reliable recovery delivery and actual decoder /
presentation evidence are still required before a new control grant.

## Verification

The request and sender/coalescer suites use production parsers, packetizers,
budgets and state machines. They cover full-picture loss, fixed failure deadlines,
stale and malformed requests, foreign owners, held decoder buffers, duplicate
floods, shared admission, reliable recovery under fresh bindings, resumed dependent
frames, and an unaffected healthy viewer. Decoder completion in these tests is
explicitly simulated, not HEVC proof.

Run with the repository-pinned compiler:

```sh
cargo test -p fr-core -p fr-wire -p fr-media --all-features --locked
cargo clippy -p fr-core -p fr-wire -p fr-media --all-targets --all-features --locked -- -D warnings
cargo test -p frd --lib media::recovery::tests --locked
```

The recovery-isolation changes (`bfe2f34`, `8a27041`, `053e659`, `cb9176d`) add
23 regressions: eight for persistent subscription limits, five for request-time
admission, five for native authority fencing, and five for expired shared cohorts.
Three of the cohort tests fail against the original implementation and pass with
the fix. The duplicate-request test makes 9,999 repeated calls without refilling
credit or extending the deadline.

Local verification with the pinned compiler passed 235 media tests across 23
targets, including the current eight-test recovery-sender suite. Five additional
authority tests pass in a targeted harness containing the exact production
admission helper and its inline tests, using the real cache/parser/authority
implementations. Another 22 existing native authority, egress and worker-supervision
tests pass, including actual child-process cleanup. Strict Clippy passes for the
media unit-test target, daemon library and targeted authority harness; changed
Rust files pass formatting checks.

All eight relevant first-party libraries were rebuilt from retained sources with
the current recovery changes against matching, unchanged Asupersync 0.5.0 and
other upstream libraries from GitHub run 35421807938 (source 38f1b0c). This is not
a cold dependency rebuild or a fresh full-workspace checkout/build. The full
daemon unit-test build exceeded the local execution limit before producing test
results; no full daemon/workspace test pass is claimed. These checks are not
native HEVC, platform-capture, network-loss or shipping-transport qualification.

## Remaining integration

Automatic control-stream routing and capability advertisement, fresh channel
reattachment, independently admitted lower operating points, and real-HEVC
injected-loss qualification remain open under `fr-p1-loss-recovery-20s`.
The chronic-viewer refusal path is implemented; the overall recovery bead is
not complete. No dependency, compiler or shipping-transport pin was changed.
