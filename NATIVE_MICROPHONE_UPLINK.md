# Native Linux microphone uplink receiver

The default-off `linux-microphone-playout` feature exposes
`fr_native::pipewire_mic::uplink::Uplink`. It joins the existing bound audio wire
records, bounded jitter buffer, isolated real Opus decoder and actual PipeWire
virtual microphone. It implements the native receiving boundary for plan
§15.4 and bead `fr-p2-audio-microphone-lz6`.

The same feature also exposes `fr_native::pipewire_mic::session::Receiver`,
which binds that real receiving boundary to the original host `InputSession`.
The containing session supplies the completed authenticated `AudioUp` attachment,
its exact positive selection, separate local microphone approval and the original
host clock. The receiver derives its permission checks from that original owner.

The original `ControlledHost` now owns the role-8 attachment and the existing
supervised receiver as part of its normal control session. The library API below
requires a separate local microphone approval and retains the original input
controller. Public `fr connect` and `frd run` microphone offers remain disabled:
visible client/host talk and stop controls are still required before enabling
those command paths. No driver or OS default source is selected implicitly.
See [NATIVE_MICROPHONE_CAPTURE.md](NATIVE_MICROPHONE_CAPTURE.md) for the actual
client recording/encoding boundary and [VIRTUAL_MIC.md](VIRTUAL_MIC.md) for
explicit virtual-source selection and the underlying native owner.

## Controlled host integration

After the original input grant, `ControlledHost::offer_microphone` takes the
original selected channel request, independent local approval and explicit
`microphone_process::Launch` paths. Missing approval or positive microphone
selection refuses before allocating a route. Normal controlled-session drive
turns complete the one-use role, binding and ticket exchange under its original
fixed deadline; callers do not manually dispatch microphone records.

The exchange retains the original controller, connection identity and positive
selection. Native processes still start only after the viewer explicitly
configures a talk generation. `attach_microphone` also accepts an already
completed original `AudioUpChannel`; a foreign or duplicate attachment is
returned intact on refusal, rather than dropped into another connection's
lifecycle. The optional slot cannot be silently replaced or reattached.

Input and its renewal are serviced before bounded microphone supervisor work.
The session retains microphone authority and pending acknowledgement checks
across connection I/O, including operations invoked by other media owners.
An active microphone prevents a bare mutable transport loan; the new
`next_channel_binding` accessor provides allocation metadata without granting
transport authority. Clipboard and file setup use that accessor.

`microphone_control` exposes the existing independent microphone stop once the
attachment completes. `retire_microphone` fences native output first and retires
the exact optional pair. A completed, drained retirement preserves desktop
control; an unfinished exchange or unwithdrawable native microphone output
conservatively fences the original connection. Session close and revocation
stop microphone admission before teardown, with no automatic talk restart.

Negotiation, native state, typed failure, bounded statistics and actual
device/codec retirement receipts are available separately. A stop request is
not a child-reaping receipt; `Running` is not proof that a local application
selected or heard the virtual microphone. This integration does not add new
physical microphone, desktop application or live-tailnet qualification.

The new controlled-session regression cases cover refusal before allocation,
fixed-deadline cancellation, original-owner fencing, optional worker failure
and native readiness/stop through the canonical drive loop. They are authored
but have not completed execution in this change's unavailable build environment.
The earlier native results below qualify the existing component owners; they
do not substitute for executing this new session composition.

## Acknowledgement and ownership

`Uplink::new` takes an explicit `Microphone`, one `AudioConfiguration`, a
nonzero channel binding, and the absolute locally installed `fr-opus-worker`
image. Configuration, direction, generation, channels, duration and resource
limits are checked before codec setup. Supported Opus durations are 10 and
20 ms at 48 kHz, with mono or stereo. There is no implicit format conversion
or reconfiguration of an existing epoch.

The existing `ProcessDecoder` owns one bounded command/reply mailbox and a
separate child for this generation. Its supervisor owns spawning, socket IPC,
timeouts, termination and actual reaping. Codec execution stays outside the
native source thread and the authority path; the source itself remains on its
original native worker thread. This is process containment, not an OS sandbox claim.
No new runtime or dependency is introduced.

`poll_ready` requires both the actual decoder configuration and a selectable
PipeWire node. `acknowledge` then builds the exact bound `AudioConfigured`
record. Its callback must admit those bytes to the original authenticated
channel before returning success. Unknown, failed or panicking admission
retires both owners; there is no acknowledgement retry or implicit resume.
Readiness is a source/codec fact, not proof that an application selected the
microphone or that the remote endpoint received the acknowledgement.

The native source and decoder cannot escape through mutable accessors.
`Control::stop` fences the exact source first, then requests decoder retirement.
The control handle neither keeps the native connection alive nor affects a
replacement owner. `decoder_retirement().is_complete()` separately reports
actual child reaping; requesting stop or dropping the owner is not described
as completed reaping.

The same state machine also accepts `Uplink<SocketDecoder>::from_socket` for
an Opus child owned by the containing supervisor. `SocketDecoder` reuses the
existing worker framing, retains one bounded request/reply, and performs at most
four nonblocking socket operations per poll. Configuration and decode deadlines
are fixed before the first write; partial messages cannot restart them.

Its `SocketFence` can only shut the exact original IPC channel. It has no process
handle or reaping method. A containing supervisor must retain the actual codec
`Child`, observe its exit, and own the device child separately. This avoids
making device teardown depend on an unobserved codec grandchild. The existing
`Uplink<ProcessDecoder>` constructor and actual retirement receipt remain
available for the original in-process native owner.

Four external-codec tests passed, including the actual sandboxed worker: six of
six independently checked tone frames matched, PLC preserved its sample timeline,
a frozen child could not block polling, and channel timeout did not claim
reaping before the parent killed/waited for the child. Separate tests covered
partial/malformed replies, fixed reply expiry and equal-ID replacement isolation.
Strict native library and focused test-harness Clippy passed. These results
qualify the external codec boundary, not a complete supervised PipeWire session.

## Original controller and independent microphone consent

`fr_media::audio::authority::HostController` captures a sealed, pure Rust
projection of the actual original `InputSession` before it moves to its input
worker. `frd::input_agent::Agent` retains it alongside the clipboard/file
projections; `QuicInput::microphone_controller` checks the original connection
and admission before handing it to the session coordinator. Its scope numbers
are metadata, and neither those numbers nor an arbitrary monitor can construct
a controller proof. Clones remain bound to the same original input owner.

`session::Authority::from_controller` consumes that projection, a completed
channel's metadata, the negotiated selection and separate local microphone
approval. The convenience `from_input` constructor delegates through the same
projection. Both refuse observation-only sessions,
missing microphone selection, missing consent, mismatched session binding and
invalid packet limits before native setup. A numeric lease or copied monitor
cannot construct this handoff. The transport coordinator still proves the
attachment's ownership and retains its one-use binding and generation floor;
channel metadata alone is not that proof.

The authority keeps the original input monitor and lease identity. A replacement
controller with equal wire IDs cannot authorize the old receiver. Its clock is
anchored before sampling the same host clock used by input, so time spent
waiting for a worker cannot restart the original authority lifetime. Every setup,
acknowledgement, receive and rendering boundary rechecks the original authority.
Actual renewals of that original input owner remain visible; the microphone
owner cannot issue a grant or manufacture a renewal.

`session::Receiver::new` validates the exact bound uplink configuration, native
Opus profile and complete datagram ceiling before creating a source or decoder.
It owns the existing `Uplink` directly, with no mutable native escape. Positive
acknowledgement requires both real owners to be ready and successful admission
on the original authenticated route. The packet API takes the original receipt
`Instant` and preserves it through the existing bounded jitter, decode and native
submission path. A shorter transport deadline must also survive the containing
worker handoff; sampling a fresh arrival at dequeue is invalid.

`session::Control` provides an independent microphone-only stop. It first fences
the native output gate and requests codec retirement, without running foreign
code or revoking input. Its permission and stopped queries sample the original
controller even when the native worker is idle. Losing or dropping the original
input owner is terminal for the microphone, as is dropping its own authority.
A matching bound `AudioStop` fences before any authority sampling or native poll.

Requested stop, physical graph removal and child reaping are separate facts.
`Receiver::state` reports native endpoint state as updated by its worker;
`Control::is_stopped` reports the policy fence. The native worker must service or
drop the receiver to destroy the endpoint, and the decoder retirement receipt
reports actual reaping. The in-process `Receiver` does not itself stop a stalled foreign call. The
supervised daemon composition below instead retains the pure original-controller
projection and passes bounded expiry/revocation envelopes to a separate native
device child. The daemon never loads `fr-native` or reconstructs an `InputSession`
in that child.

## Supervised host device and codec

`frd::microphone_process::Process` owns one explicitly approved uplink epoch.
It validates the original `HostController`, independent microphone approval,
positive uplink selection, completed attachment metadata and exact configuration
before launching native work. `Launch` accepts only explicit local absolute
paths for the installed `fr-microphone-worker`, `fr-opus-worker` and PipeWire
socket. No source, default device or public capability is selected implicitly.

A bounded supervisor thread owns both actual OS child handles. The device and
sandboxed codec are siblings: the device uses `Uplink<SocketDecoder>` and cannot
spawn an unobserved codec grandchild. Their command, codec and terminal-revoke
sockets are separate. The device installs parent-death binding before native
setup; both images start with a cleared environment. The daemon contains only
pure policy, framing and process supervision, with no native audio dependency
or second async runtime.

Configuration has two distinct acknowledgment stages. The private child reply
proves actual codec readiness and a selectable virtual source. The parent checks
its exact `AudioConfigured` bytes against the offer, then invokes the original
route's admission callback once. Only successful network admission followed by
a fresh authority check permits the private `Commit`. Packet admission opens
only after the matching commit reply; failed, panicking or unknown admission
fences the original owner. This is transport admission, not remote delivery or
proof that an application heard sound.

The supervisor checks the original controller while idle and during every
exchange. Renewed worker authority must be acknowledged before the predecessor
cap and the fixed exchange expiry. The device retains that earlier bound while
its reply is pending, checks it inside native service, and checks again after
the final reply write. A delayed exchange cannot revive an expired worker.
Local microphone stop closes the dedicated revoke gate without revoking input.
Original-owner loss, malformed admitted records, native failure, closed IPC and
stalled exchanges all retire the same epoch; there is no automatic restart.

One packet may be queued or awaiting collection at a time. Its original local
receipt and earlier expiry are converted conservatively to the same-boot clock
before entering that inbox. The device uses `receive_record_until`, retaining
the smaller transport cap through jitter, codec and native PCM custody. Health
polls and controller renewals never retime old audio. Snapshot counters report
bounded local custody and exact node identity, not audible delivery.

At most four supervisors are admitted globally, including cancelled owners
whose children have not been reaped. Retirement signals both children before
waiting for either and records each actual OS wait separately. `Retirement`
distinguishes not-started, running, reaped and unresolved children; socket EOF,
a stop request or a missing graph node is not a reaping receipt. An unresolved
wait retains its admission slot. Read-only child IDs are copied from the original
OS handles and kept as diagnostic metadata; stop and reaping never act on a
caller-supplied PID. The original controller and separate local approval remain
mandatory when integrating this owner into the network session.

## Native graph demand and independent clocks

An Opus frame contains 480 or 960 samples per channel. PipeWire graph quanta
need not have either width. The receiving owner therefore uses real native
graph demand rather than packet arrivals or submitted-frame counters as an
audio clock.

`Microphone::poll_demand` selects paced mode. A source without a running consumer
returns no demand, and the receiving owner ignores audio packets in that state.
It does not accumulate sound while waiting for an application to select the
source. Once streaming, a demand token names the original endpoint, generation,
native cycle serial, actual consumed graph position and one future output
interval. Repeated reads do not extend the token's original deadline.

The source schedules two graph quanta ahead: one for the non-real-time worker
and isolated decoder, and one for the asynchronous native handoff. A service
turn admits or collects at most one decode. One exact-next real packet can
decode ahead while previously submitted PCM spans several graph quanta. Its
reply stays in the existing single process mailbox; there is no second PCM
FIFO. Concealment is never generated ahead of native demand.

The first output anchor is chosen only after real PCM is ready, under the
original first-arrival reorder wait and packet expiry. Every subsequent output
position is fixed by the preceding accepted span. Ready PCM acquires a native
token only when that token contains the fixed position. If a callback has
already passed it, the epoch retires; a later token cannot move it forward.
Pending concealment retains the exact token under which it began. Original
source positions, drift adjustment and receipt expiry survive every decoder
poll. No codec work runs in a PipeWire callback.

The fixed native queue splits or joins decoded PCM across real graph quanta.
For example, 480-sample frames retain their exact remainder across a
512-sample graph interval. Remainders keep their original graph position and
wall expiry. Once sound starts, a missing interval, pause, clock discontinuity,
rate/quantum change or expired retained sound retires the epoch. Graph requests
larger than 1,024 frames are explicitly unsupported by this four-block budget.
The feature never changes the user's graph quantum to fit the implementation.

Source samples and output samples remain separate timelines. The existing
bounded drift estimator observes uniquely admitted source timestamps against
the actual local graph cursor. Its target includes the negotiated jitter delay
and the fixed callback/handoff allowance. It never uses a submitted packet count
as evidence of consumed audio. The existing ±1,000 ppm correction can produce
479/481 or 959/961 output samples; the source timeline still advances by the
nominal 480 or 960 samples. Fractional correction credit is committed once,
only after native queue admission succeeds.

## Original deadlines and bounded retention

`receive_record_at` accepts the original local network receipt `Instant`,
before a bounded worker inbox. This is not a remote wall clock or the dequeue
time. The 100 ms receipt budget includes jitter retention and decoder work;
it is not a claim about physical capture latency or network transit time.
Future receipt times are refused. Expired arrivals are ignored without
refreshing startup or retained work. `Uplink::receive_record_until` and the
corresponding `session::Receiver` method also accept an earlier explicit
transport expiry. The accepted deadline is the minimum of that bound and the
original receipt's 100 ms ceiling; matching bound stop still fences first.

`QuicRecords::receive_ready_with_receipts` exposes each datagram's original
application receipt and inclusive expiry as immutable `DatagramReceipt`
metadata. Blocked callbacks, audio overflow promotion and worker handoff do not
restart either timestamp. The values use the original connection's Asupersync
timer-driver microseconds; they are not capture times or native socket arrival
evidence. A clock conversion must preserve both bounds conservatively, including
any transport allowance shorter than 100 ms. Reliable stream records carry no
datagram receipt. Existing receive callbacks remain available unchanged.

The focused transport suite passed all 31 tests, including five new receipt
custody regressions, using the exact production custody source and pinned
Asupersync byte-buffer source. Strict scoped Clippy passed. The actual UDP/TLS
route-isolation fixtures now also assert immutable receipt delivery; those
fixtures have not been rerun in this resource-constrained environment.

Binding, direction, generation, packet length and negotiated decoded duration
are validated before copying payload into the jitter queue. The shared jitter
buffer has 16 fixed slots and a 100 ms source-span ceiling. Original receipt
expiry metadata has the same fixed capacity. Duplicate and late records cannot
replace the original expiry; startup eviction removes the corresponding
metadata. The startup reorder deadline is anchored to the first timely admitted
record's original receipt, rather than construction, acknowledgement or polling.

There is at most one outstanding process decode, four private PCM blocks in
the native source and one local PCM value being checked/submitted. PCM in the
receiving owner is erased on every completion or refusal. Concealment uses the
existing decoder history and shared jitter limits, preserves the nominal
source timeline, and cannot extend the original last-real-packet budget.

The native boundary independently checks the generation, source continuity,
graph interval, fixed queue capacity, adjusted sample count, original wall
expiry and terminal permission fence. It checks permission and freshness again
immediately before publishing the prepared native buffer, including a bounded
presentation tail. Successful `RenderResult::Submitted` means bounded local
custody; native submitted counters mean queued to PipeWire. Neither result
claims that an application heard the sound.

A matching bound `AudioStop` fences the native source before any permission
predicate or native polling, regardless of record age. Permission failure or
panic, malformed admitted records, failed acknowledgement, missed demand and
codec failure retire the same original owner. Replacement requires fresh
session admission and a fresh generation; this API has no reconnect or resume
operation.

## Native failure evidence

The existing native `Statistics` now retains a fixed 64-byte first-failure
record through endpoint cleanup. Its code, operands and graph timestamps
identify the first failed native check without retaining sound or error strings.
There is no callback logging, allocation or second queue. The paired C header
lists every local code; these values are diagnostic metadata, not wire protocol.

This distinguishes actual PCM underfill, stale graph snapshots, nonconsecutive
native callback cycles and an interpolated sample regression. The last completed
callback and a later demand read's sample estimate are recorded separately.
Diagnostics do not relax the native interval, final publication, packet expiry
or permission checks, and they never turn requested retirement into evidence of
completed cleanup.

## Executed native evidence

On October 31, 2026, all eight checked-in PipeWire integration cases passed
against the actual package, PipeWire 1.0.5 and verified libopus 1.5.2. An
external manifest references the repository sources directly, avoiding the
unrelated heavyweight development dependency graph in the constrained local
build environment. The codec worker is the checked-in worker binary, with a
test-only library search path in that image; no production source or codec
interface is substituted.

Both 10 ms and 20 ms real Opus uplinks delivered the synthetic 500 Hz fixture
to an independent `pw-cat` recorder. Each had 148,800 frames matching the
spectral oracle; the final quarter second was silent, and an independent node
inventory confirmed removal of the exact source. The deliberately frozen
decoder case recorded 7,168 frames of silence, retired without publishing
delayed sound, and confirmed actual child reaping. The other cases include
acknowledgement and binding refusal, original receipt expiry, immediate bound
stop, and the existing raw-source permission, stop, expiry and panic regressions.
Strict library and test-harness Clippy also passed.

Qualification found and corrected two timing mistakes: an Opus frame cannot
wait to begin decoding until a short graph interval needs it, and a demand
token's admission deadline cannot become the expiry of every carried PCM
fragment. Exact-next real-packet predecode preserves fixed output positions;
accepted fragments retain their original receipt expiry separately from token
admission. Neither correction renews a packet lifetime or rebases missed sound.

This evidence covers synthetic encoded records and a private native graph. It
does not establish physical client microphone consent, a network role-8 path,
interactive talk controls, every hardware/graph configuration, or measured
capture-to-application latency.

### Original-controller receiver coverage

The pure daemon/native handoff passed four ownership tests and four native
receiver policy tests, with strict `fr-media` library and targeted-test Clippy.
The tests move the real input owner to a worker thread, check equal-ID replacement
isolation, genuine original-owner renewal, inclusive terminal expiry, separate
microphone consent and microphone-only stop. The new daemon input-agent handoff
regression is authored but not executed locally because its Asupersync build
exceeds the available build-memory budget. These checks do not change the native
graph qualification limits below.

The new original-controller owner passed its three policy/clock tests and
strict library and exact-source harness Clippy. Those tests cover independent
consent, observer and binding refusal before native setup, original owner drop,
equal-ID replacement isolation and setup delay spending the original lease.

The actual controller-revocation recorder scenario completed twice with the
unchanged independent 500 Hz oracle. One retained run recorded 184,832 frames,
including 148,800 frames matching the audible oracle, with zero final-tail peak
and removal of the exact virtual source. These checks use a real `InputSession`
policy fixture and real PipeWire/Opus processes; they do not establish native
approval UI or network admission.

The three new native fence scenarios did not achieve a clean qualification run
in this environment. Later runs stopped before the controller-revocation,
controller-drop or microphone-only-stop step because existing graph checks
refused PCM underfill (160 of 256 required frames), a stale graph snapshot
(546 elapsed samples against a 512-sample bound), or a skipped callback interval
(512 rather than 256 samples). One measured run had CPU throttling; the final
run did not, so driver/callback timing remains an unresolved qualification
limit. No interpolated sample regression was observed, and no timing guard or
recorder assertion was relaxed.

Controller-drop and microphone-only-stop native acceptance therefore remain
unqualified, and the earlier successful revocation observations are not a claim
that sustained playout is reliable on this runner. The tests and content-free
failure evidence are retained for qualification on the target native graph.
Public microphone offers remain disabled.

### Supervised sibling-process coverage

All six parent tests passed against the exact production parent module; five
launched the actual standalone device and sandboxed codec. They establish real
native readiness, exact route-acknowledgment ordering, bounded packet custody,
terminal malformed/future-receipt refusal, idle original-controller revocation,
frozen-device timeout, frozen-codec retirement and equal-ID replacement
isolation. Both original child handles were actually waited for, and independent
`pw-dump` queries verified the same source's presence before each lifecycle
trigger and absence afterward. Owner-drop cases attributed retirement to
`Revoked`, within 500 ms and before the saved original authority deadline.

All 17 private IPC tests passed, covering bounded body/header parsing, exact
scope and custody matching, partial I/O and original inclusive deadlines. The
four codec/socket regression tests also passed again, including the actual
worker's six of six checked tone frames and the PLC sample timeline. Strict media/native library and focused
harness Clippy passed. These tests use controller policy fixtures and a route
admission callback fixture; the new lifecycle cases deliberately have no audio
consumer. They qualify process ownership and cleanup, not sustained audible
playout or a complete authenticated network session.

The isolated test images resolve verified native libraries through a test-only
RPATH. A local device launcher supplies the verified extracted PipeWire module
and configuration paths, then execs the actual device image in the same PID.
Production does not forward that environment or use the test launcher. The
existing native CI step also runs the new parent suite using exact worker
artifacts from its all-feature build. Full workspace/runtime compilation remains
blocked locally by the available build resources; no complete workspace result
is inferred from the focused harness.

## Qualification commands

The native integration tests use a private PipeWire daemon and independent
recording process. They reference the actual `fr-native` package, existing
encoder and checked-in `fr-opus-worker` binary. The normal repository command is:

```sh
cargo test -p fr-native --no-default-features \
  --features linux-microphone-playout --test pipewire_microphone --locked -- \
  --include-ignored --test-threads=1 --nocapture
```

The separate supervisor suite takes explicit absolute paths to the installed
worker images. With Cargo's default target directory, build and run it with:

```sh
cargo build -p fr-native --no-default-features \
  --features linux-microphone-playout \
  --bin fr-microphone-worker --bin fr-opus-worker --locked
FR_TEST_MICROPHONE_WORKER="$PWD/target/debug/fr-microphone-worker" \
FR_TEST_OPUS_WORKER="$PWD/target/debug/fr-opus-worker" \
cargo test -p frd --test microphone_process --locked -- \
  --include-ignored --test-threads=1 --nocapture
```

Adjust the two absolute image paths when using another target or installation
directory. The tests fail on missing native dependencies; they do not silently
skip a requested native run.

Required native packages include PipeWire, its standard modules and tools,
Python 3, and a qualified libopus with the required decoder-complexity control.
The codec child deliberately clears its environment. Its installed image must
resolve the qualified protected native library itself; a parent-only
`LD_LIBRARY_PATH` does not establish that property. Test-only isolated library
paths must not be mistaken for a shipping installation policy.


### Supervised host-consumer delivery and native graph cadence

The native source requests `512/48000` graph latency. The previously requested
`480/48000` is rounded down to a 256-frame quantum by the qualified PipeWire
1.0.5 default power-of-two policy, so it requested roughly 5.3 ms in practice.
The [pinned graph scheduler](https://github.com/PipeWire/pipewire/blob/1.0.5/src/pipewire/context.c)
applies `flp2` after selecting the latency. A 512-frame suggestion requests the
intended approximately 10-ms cadence without forcing or locking the shared
host graph. This is a real native profile choice, not permission to assume a
512-frame clock: every actual graph interval, clock identity, discontinuity,
queue bound, final publication check and original 100-ms packet ceiling remains
independently enforced. A different consumer or host policy can still select a
different quantum, which must pass those same checks.

On November 1, 2026, the unchanged 480-frame profile delivered the direct 10-ms
fixture but a direct 20-ms run stopped at the strict stale-snapshot check
(native code 7, 514 elapsed samples against a 512-sample bound). A subsequent
supervised run ended during initial silent graph service. These are retained
negative results; no assertion, deadline, or native guard was relaxed.

With the corrected latency suggestion, the complete exact-source PipeWire test
binary passed all 16 cases with native cases explicitly enabled. The two direct
Opus durations each produced 148,800 independently verified 500-Hz samples.
The original-controller revoke, owner-drop, microphone-only stop, frozen-codec,
raw-source expiry, and permission-panic cases also passed.

The new `session::supervised` case feeds actual Opus records through the daemon's
exact `microphone_process` implementation and its two separately owned native
children, into a real virtual source selected by an independent `pw-cat` host
consumer. Original controller renewals sustain the same children beyond the
initial authority cap. The case then drops that original input owner with a
fresh final tone queued for native admission, observes both real OS reaping receipts
without another
process/network service turn, and requires the terminal reason to be original
authority revocation. Independent recording observed 185,344 frames, including
144,000 matching the 500-Hz spectral oracle, a zero final-quarter-second peak,
and removal of the exact source. Stop receipts remain complete after repeated
terminal requests.

The recorded signal and input establishment are fixtures. This is evidence of
actual supervised host-application delivery and original-owner withdrawal; it
does not establish physical client microphone permission, the complete network
path, public talk controls, or capture-to-application latency. Native queue
submission counters remain distinct from independently recorded audio.
