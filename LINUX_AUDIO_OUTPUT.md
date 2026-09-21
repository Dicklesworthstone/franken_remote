# Native Linux audio output

`fr-native`'s opt-in `linux-pulse-playback` feature adds a real libpulse playback
boundary. Select a local UNIX server socket and an explicit output name; there
is no TCP server, default-output alias, autospawn, microphone or device fallback.
The caller must already hold local enable and observation approval. Supported
output frames are 48 kHz native-endian signed i16, mono/stereo, 10/20 ms downlink.
Other configurations refuse before native allocation, not silently resample.
The selected system server may perform its ordinary hardware format conversion;
this is not a claim of a direct, bit-perfect hardware path.

`PlaybackDevice::connect` starts silent configuration. `poll` performs at most
four nonblocking native mainloop turns. It opens one stream and retains at most
one bounded completion operation, with stable callback userdata and unique native
ownership. No additional Rust runtime, application thread, broad binding crate,
server downloader or shell audio process enters production. The adapter belongs
on the supervised audio worker, not the authority thread or realtime callback;
native calls can still hang and require external process supervision.

The requested per-stream queue and combined sink/stream target are capped at 40 ms. The
actual returned format, buffer attributes, suspension state and device index are
checked, not inferred from accepting the request. A changed device or buffer
contract retires the stream. Resource accounting here covers the stream's audio
queue and bounded first-party metadata; libpulse's native pool/connection storage
and the system server/device's shared allocations are separate trust/accounting
boundaries, not claimed limited to that small queue.

Playback time comes from libpulse's interpolated device timing, not received
packet count or a fabricated wall-clock sample counter. There is one timing
request at a time. Timing expires after 50 ms measured from the original request,
so a delayed reply cannot refresh stale evidence. This is the native server's
clock estimate, not instrumented sound-card/audible timing qualification.

`submit` takes the existing bounded PCM and AudioSubmission metadata. It checks
the direction, epoch, exact negotiated shape, source timestamp, sequence, output
slot and original packet expiry. The actual server-configured sink latency plus
5 ms selects a scheduling lead (at most 20 ms) on first submission; that lead then
stays fixed for the stream. A later latency change cannot silently shift submitted
audio. Absolute sample offsets avoid appending a stale backlog after an underrun. The complete scheduled interval must fit the original
expiry and the device queue ceiling. Permission is checked again at the native
submission boundary. A native write error is terminal/unknown, never retried.
Success means the OS-facing libpulse API accepted its synchronous copy, not that
the server, hardware or a human has audibly observed it. No PCM is logged.

Stop fences new writes immediately. The graceful path corks then flushes the
stream under one original 100 ms deadline and distinguishes acknowledged flush
from forced disconnection. Error, revoke, cancellation and caught callback panic
disconnect the unique stream/context before their native mainloop storage dies.
No server-wide mute or other application's audio is changed. Already rendered
hardware samples cannot be retracted; there is no instantaneous audible-silence
claim. Replacement requires a newly admitted audio generation in the session.

## Verification checkpoint

The exact production library builds and passes strict pedantic Clippy using
nightly-2026-08-31. Three source tests pass for selection/format/direction bounds
and original output-slot/expiry arithmetic. One native failure-path test passes
64 real libpulse connect attempts against an absent local socket, without
starting a server or substituting a device. The complete new integration-test
source also compiles and passes strict pedantic Clippy.

The real-server tests are checked in, not yet counted as passed at this first
checkpoint. They launch a private PulseAudio server with a null output and an
independent libpulse-simple monitor. They check native clock progress, actual
PCM arrival, bounded queues, acknowledged cork/flush, revocation/panic, stale
clocks and server disappearance. No user's sound device or default microphone is
used. `scripts/verify-pulse-playback.sh` selects the actual production dependency
paths and exact test source in an external manifest to avoid unrelated daemon/
HEVC development dependencies; it does not replace any first-party code. The
matching CI job runs those tests on Debian 13 and retains authenticated-package
native inputs for offline revalidation. Missing server prerequisites fail rather
than silently skip. Physical devices, audible latency, PipeWire compatibility,
session/worker orchestration and capture remain unqualified/open.

Refs: plan 15.4 and fr-p2-audio-playback-lel. The native ABI/semantics were checked
against PulseAudio 17's [stream.h](https://github.com/pulseaudio/pulseaudio/blob/v17.0/src/pulse/stream.h)
and [def.h](https://github.com/pulseaudio/pulseaudio/blob/v17.0/src/pulse/def.h).
See [clock-paced playout](AUDIO_PLAYOUT.md) and [native Opus](NATIVE_OPUS.md).


## Bound wire-to-device playback

`pulse::playout::PulsePlayout` owns the admitted FRD0 binding, real Opus receiver
and selected native output together. Construction requires exact agreement between
the remote offer and actually configured device, then creates the real bounded
Opus decoder and rechecks local permission. Neither owner can be extracted through
a mutable escape. This adds no thread, new runtime, listener or implicit audio grant.

`acknowledge` emits the actual `AudioConfigured` record through one caller-owned
nonblocking authenticated-channel send. It uses the device's real configured
format and runs only after both native device and decoder succeeded. A single
original two-second acknowledgement deadline includes native preparation after
entry; repeated calls cannot extend it. It must succeed before incoming packets
or decode. The caller's success means local send admission, not remote receipt;
partial/unknown send and callback panic retire both native owners. Repeated ACK
refuses without calling the sender again. The surrounding session must retain
its own original admission/startup deadline as well.

`service`, `receive_record` and `render` use the actual output's native timing.
`render` performs at most one decode/PLC and one native write; the packet's original
source identity, epoch, duration, device slot and expiry reach that final boundary.
Local permission is checked before/after codec work and again at the native write.
Existing volume/mute acts on future PCM; no claim is made that an already queued
frame was retroactively muted. Video is not held for this audio loop.

A matching bound `AudioStop` first fences packet/decoder work, before any native
mainloop work or permission callback. Cleanup then corks/flushes the device even
though observation permission has ended. Late packets cannot reopen the stream
or abort the original flush. Stale direction/epoch stops are ignored. Any unknown
native write, malformed Opus, output failure or caught callback panic drops both
owners' pending work. `poll_stop` performs cleanup only, never decodes or writes.
Replacement requires a newly admitted epoch in the surrounding session.

## Actual timing, native capacity, and qualification results

The first device-only CI run at dc53b94 passed four tests and failed two. A cold
null sink had nearly two seconds of already queued device silence although its
returned stream attributes met the requested limits and get_latency() reported
zero. Configuration acceptance was therefore not evidence of a running, fresh
clock. The implementation now checks the real public native timing record, actual
sink delay and 40 ms of clock progress within a 2 ms phase envelope before Ready.
All attempts share the original two-second startup deadline. It injects no dummy
PCM and invents no sample counter. The original cold, high-delay sink remains an
explicit negative test; positive output tests use the separately selected native
low-latency null-sink profile (`norewinds=1`). Neither qualifies physical hardware.

Native writable_size is request credit, not free capacity. It may be less than
one complete negotiated frame, or grow during freewheel silence. The adapter now
checks the absolute scheduled byte extent against the last server-reported read
position, not that advisory credit alone. The combined server/native/socket sample
extent is capped at the 40 ms server queue plus one frame of in-flight allowance:
at most 60 ms / 11520 bytes for 20 ms stereo. This includes outstanding writes
regardless of which native layer retains them. It is separate from the current
bounded PCM and encoded jitter storage; the native library's auxiliary pool and
system-wide audio allocations remain explicit, unqualified trust/accounting limits.

The final normal native run passed all **13 tests, zero failed or ignored**: seven
device tests and six wire/Opus/device composition tests. Actual independently
captured monitor PCM verifies output, stereo layout and silence after acknowledged
stop. Composition exercises the four 10/20 ms mono/stereo profiles, actual Opus,
deliberate loss and PLC, duplicates, exact configured ACK, malformed payloads,
revocation, panic cleanup, generation/binding fencing, and retained stopped owners.
The independent monitor is configured before output qualification, because adding
an observer can itself change shared native sink scheduling. No PCM is logged.

The attempted 5 ms native profile intermittently missed its whole device slot.
It now explicitly refuses before native allocation; the underlying Opus codec's
5 ms support is unchanged. This is a recorded qualification failure, not a silent
coalescing/duration fallback or a claim that every Opus profile works on this output.
The 10/20 ms positive assertions still enforce their original slots and deadlines.

Four source/limit tests pass, including native outstanding-extent boundaries. The
six final composition tests also pass Rust AddressSanitizer with leak detection.
The device-only sanitizer run passed six and failed the PCM-output test with a
Clock refusal. Earlier normal runs also produced timing-sensitive Clock and
Backpressure refusals; these negatives are retained and do not become hardware or
stress qualification because a later normal run passed. System libpulse, libopus
and the private daemon are not sanitizer-instrumented; no full-native-memory-safety
or all-sanitizer-green claim is made. No production deadlines were relaxed.

The final relevant shared suite passes **832 tests, zero failed or ignored**.
Exact production and complete native test-target strict pedantic Clippy, formatting,
whitespace and shell syntax pass. Every first-party dependency is source-built
with nightly-2026-08-31. Native test packages came from the authenticated Debian 13
APT resolution retained by GitHub run 35645681083/artifact 10660620423; ZIP SHA-256
445189e4959305cbda07eaa000e00c41de3de45093eabf98336082bd6162cc6b and all extracted
package hashes were checked. Local native tests ran as an unprivileged user against
actual libpulse 17 and libopus 1.5.2. The public pa_timing_info layout/semantics were
checked against the same libpulse-dev headers. The production adapter does not
download these packages, launch a test server, load arbitrary libraries or capture
the user's microphone.

Per-OS host capture, the containing authenticated audio channel and worker/session
service, external hang supervision, physical output, PipeWire compatibility,
audible latency, sustained timing stability and cross-host A/V alignment remain
open. This is a real native worker-level receive-to-output slice, not an installable
remote-audio application or closure of fr-p2-audio-playback-lel.
