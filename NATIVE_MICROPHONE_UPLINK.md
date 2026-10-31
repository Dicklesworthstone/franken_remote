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

Public microphone offers remain disabled. These native owners do not attach
protocol role 8 themselves, change `fr connect`, install a driver, or choose an OS
default source. Network coordination and visible talk/stop controls remain
integration work. See [NATIVE_MICROPHONE_CAPTURE.md](NATIVE_MICROPHONE_CAPTURE.md)
for the actual client recording/encoding boundary and [VIRTUAL_MIC.md](VIRTUAL_MIC.md)
for explicit virtual-source selection and the underlying native owner.

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

## Original controller and independent microphone consent

`session::Authority::from_input` takes a reference to the actual original
`InputSession`, a completed channel's metadata, the negotiated selection and
separate local microphone approval. It refuses observation-only sessions,
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
reports actual reaping. The PipeWire owner still needs outer process supervision
before integration into the daemon; this API does not claim to stop a stalled
foreign call by itself. This receiver is an in-process native composition,
not the future child IPC contract. The daemon must keep its original controller
projection in a pure Rust module and pass bounded expiry/revocation envelopes to
a native child; it must not load `fr-native` or reconstruct an `InputSession` in
that child to use this API.

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
refreshing startup or retained work.

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

## Qualification commands

The native integration tests use a private PipeWire daemon and independent
recording process. They reference the actual `fr-native` package, existing
encoder and checked-in `fr-opus-worker` binary. The normal repository command is:

```sh
cargo test -p fr-native --no-default-features \
  --features linux-microphone-playout --test pipewire_microphone --locked -- \
  --include-ignored --test-threads=1 --nocapture
```

Required native packages include PipeWire, its standard modules and tools,
Python 3, and a qualified libopus with the required decoder-complexity control.
The codec child deliberately clears its environment. Its installed image must
resolve the qualified protected native library itself; a parent-only
`LD_LIBRARY_PATH` does not establish that property. Test-only isolated library
paths must not be mistaken for a shipping installation policy.
