# Bounded reference recovery

The delivery layer implements `RecoveryRequest` (`0x0036`, protocol section 6),
per-subscription sender fencing, and one fixed-space shared-encoder IDR admission
queue. Native subscriptions also join accepted requests to their actual input
authority and capture worker. This is not yet automatic control-stream routing,
channel reattachment or hardware qualification. The application must negotiate `reference-recovery` version 1,
route requests through the original admitted control connection, and perform the
existing fresh-binding and decoder recovery handshake.

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
the earliest cohort deadline. Its configured 100 ms to 1 s minimum interval
bounds enqueue rate, including failed enqueue attempts. New generations and
cancellation do not replenish the rate allowance. A different viewer's request
never resets healthy bindings or decoder state. The actual native owner must
check authority, fence stale-view input, and enqueue force-IDR work within the
returned deadline; this delivery API is not itself an input grant or an encoder.

## Native authority and capture

`Subscription::request_recovery(source, bytes, installed_binding)` joins an
accepted request to the actual `CaptureSource` which supplied that subscription.
A foreign worker with equal frame/configuration IDs cannot receive the demand.
The installed session identity is checked under the original shared authority
mutex. After bounded request admission, view readiness and old input tickets are
invalidated under that same mutex, before any native work; malformed requests
leave a healthy view alone. No authority lock is held across IPC or an await.

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

## Verification and remaining integration

The two request test suites and sender/coalescer suite use production parsers,
packetizers, budgets and state machines. They cover full-picture loss, fixed
failure deadlines, stale and malformed requests, foreign owners, held decoder
buffers, duplicate floods by repeated calls, shared admission, reliable recovery
under fresh bindings, resumed dependent frames, and an unaffected healthy viewer.
Decoder completion in these tests is explicitly simulated, not HEVC proof.

Run with the repository-pinned compiler:

```sh
cargo test -p fr-core -p fr-wire -p fr-media --all-features --locked
cargo clippy -p fr-core -p fr-wire -p fr-media --all-targets --all-features --locked -- -D warnings
```

The core/wire/media slice passes 507 tests including doctests and strict Clippy
using locked offline dependency sources. Native integration adds five tests with
real child processes and the production IPC/authority/capture owners; all five,
plus 22 existing authority/egress/worker regressions, pass. Strict Clippy passes
for the daemon library and new native tests. Test child payloads are deliberately
not HEVC and are not platform or codec evidence.

Native local checks rebuild all eight relevant first-party libraries with the
pinned compiler against unchanged, matching Asupersync 0.5.0 and other upstream
libraries retained by GitHub run 35421807938 (source 38f1b0c). Archive hashes were
verified; no dependency pin was changed. This is not a cold dependency rebuild.
The same first commit passed 59 transport integration tests and a workspace
Cargo check in that CI run; workspace Clippy stopped in untouched desktop
reconnect tests. A broader local daemon unit-test build exceeded the execution
limit before producing test results, so no complete daemon/workspace test pass
is claimed.

Automatic control-stream routing and capability advertisement, fresh channel
reattachment, chronic-viewer refusal policy, and real-HEVC injected-loss
qualification remain open under `fr-p1-loss-recovery-20s`.
