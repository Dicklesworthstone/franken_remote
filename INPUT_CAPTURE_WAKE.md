# Input-driven idle capture

The controlled streaming host now uses collected native input results to prompt
one early source check while adaptive capture is in verified idle. This connects
`ControlledHost`'s existing canonical input owner to `StreamingHost`'s real raw
capture admission. It adds no input protocol, executor, codec or runtime.

## What triggers a check

The signal comes from a newly collected native result with a nonzero submitted
operation count and a submitted/observed stage. Packet receipt, an admitted but
unfinished command, a zero-effect refusal, ticket delivery, challenge responses
and reconciliation do not create the signal. Submission establishes an OS-API
submission, not an application response, changed pixels or physical visibility.

Separate monotonically retained action and pointer positions reject immediate
and older replays, including replay after an intervening refusal. The marker
holds only those positions, not keys, coordinates, text, credentials or media.
It belongs to the existing input owner; no second replay or execution ledger is
created. Native result and cleanup semantics remain unchanged.

Collection does not wait for the reverse input receipt to enter QUIC. A
backpressured receipt remains owned by the original input attachment and retains
priority over subsequent commands. The same retained receipt cannot repeatedly
trigger capture checks.

## Bounded scheduling, not invented freshness

Enable the existing opt-in adaptive capture before serving the controlled host:

```rust
host.enable_adaptive_capture(std::time::Duration::from_millis(200))?;
```

Fixed pacing is unaffected. During active capture the normal controller already
sets admission cadence; the wake signal is discarded rather than saved for a
later idle period. In verified idle, actual new submissions coalesce into one
hint that expires 250 ms after the latest collected submission. There is no queue
of capture opportunities and no catch-up burst.

An early capture requires all of the following on the current service turn:

- No queued, executing or uncollected capture; the existing single credit remains
  exclusive. An already-issued operation is never cancelled by input.
- The configured minimum capture interval has elapsed since the preceding raw
  admission. Input cannot exceed the already-qualified frame-rate ceiling.
- Actual sender admission and cache capacity are ready, with recently measured
  source work below the existing pressure threshold.
- When decoder metrics were negotiated, a timely measurement says the receiver
  is empty and not decoding, with no known excessive service time. Missing
  receiver evidence blocks early capture.

An empty, measured receiver can qualify for this one source check after its old
completed decoder-duration measurement has aged out during idle. This does not
turn missing duration evidence into continuous-rate headroom; the controller's
upward-probe conditions remain unchanged. Unnegotiated sessions retain the
existing local-only pacing behavior.

The normal scheduled capture remains independently available. Either a normal
or an early admission consumes the hint. Send/cache/decoder pressure can delay
or suppress the early check; the implementation does not guarantee immediate
visual response or override an existing resource or authority limit.

The check uses the original supervised `capture_if_changed` operation. Only its
actual result updates changed/unchanged source evidence. A key that changes
nothing can produce an unchanged observation without an encoded frame, and the
controller remains in idle. All encoded references, pending packets, timestamps,
input identities and absolute deadlines are preserved.

`Statistics::input_wake_captures` counts actual raw admissions made before the
ordinary idle deadline. It does not count packets, commands, delivered frames,
application responses or refreshes of input authority.

## Verification

Seven deterministic tests cover independent sequence spaces, replays and
refusals, the exact admission floor, coalescing, in-flight work, pressure causes,
exclusive hint expiry, fixed/active pacing, clock faults and an empty receiver
whose previous decoder timing has expired.

Three runtime tests use actual localhost UDP/TLS, negotiated channels and the
canonical native owner. The OS sink and conditional codec subprocess are
explicitly synthetic. Two exercise the real streaming loop beyond the original
three-second lease: one collected key submission advances exactly one genuine
idle source check before the normal 200 ms opportunity, with zero encoded
pictures; the fixed-policy counterpart admits no wake captures. Both preserve
receipts, ticket renewal, release-only shutdown and exclusive seat cleanup.

The third blocks an entered native submission and saturates the original reverse
QUIC input stream. Unfinished input cannot signal activity. After actual result
collection, exactly one signal appears while its receipt remains backpressured;
repeated maintenance neither consumes that receipt nor replays the signal.

A deliberately disconnected source copy fails the unchanged end-to-end wake
regression: the next source observation arrives on the ordinary approximately
200 ms cadence instead of the asserted sub-150 ms test boundary. This is a test
of the production scheduling connection, not a claimed physical latency number.
Exact raw-admission floor arithmetic is separately deterministic; source-service
timestamps include variable processing delay and are not raw-admission instants.

All ten new tests passed at one, four and eight test threads. The final local
selection passed 132 ordinary daemon tests. Thirteen existing explicit real
FFmpeg HEVC/X11 streaming regressions also passed during this implementation;
they cover the surrounding media paths, not a new hardware input-wake claim.
Three existing network-namespace cases remain unexecuted. First-party runtime
sources were freshly rebuilt with the pinned compiler against matching retained
dependencies, not a cold local whole-workspace dependency build. Committed-source
CI is reported separately from those local results.

## Remaining scope

This is the collected remote-input wake path, not an OS damage notification or a
local physical-input hook. It does not implement bitrate/resolution adaptation,
aggregate path budgets, qualified listener ingress, native consent UI or physical
presentation callbacks. Those parts of the broad adaptive/application work remain
open. It never turns an input result into source freshness or observation/control
permission.
