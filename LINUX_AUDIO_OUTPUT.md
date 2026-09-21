# Native Linux audio output

`fr-native`'s opt-in `linux-pulse-playback` feature adds a real libpulse playback
boundary. Select a local UNIX server socket and an explicit output name; there
is no TCP server, default-output alias, autospawn, microphone or device fallback.
The caller must already hold local enable and observation approval. Supported
output frames are 48 kHz native-endian signed i16, mono/stereo, 5/10/20 ms downlink.
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

The requested per-stream queue is capped at 40 ms, with a 20 ms target. The
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
slot and original packet expiry. A fixed 20 ms scheduling lead keeps writes ahead
of the consumed sample position; absolute sample offsets avoid appending a stale
backlog after an underrun. The complete scheduled interval must fit the original
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
