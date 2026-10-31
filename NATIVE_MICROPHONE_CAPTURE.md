# Native Linux microphone capture

The opt-in `linux-microphone-capture` feature provides
`fr_native::pulse::microphone::MicrophoneSource`: an actual locally selected
PulseAudio RECORD stream feeding the existing real libopus encoder. It is the
client capture boundary for plan §15.4 and bead `fr-p2-audio-microphone-lz6`.
It does not enable a network microphone capability or change `fr connect`.

## Selection, consent and lifecycle

The caller supplies an absolute Unix socket, an explicit source name, an
uplink `AudioStreamConfig`, bitrate, payload ceiling, and the original
session's permission predicate. Source names and socket paths are local
configuration, never peer-supplied metadata. There is no default-source alias,
server fallback, autospawn, source creation, or default-device change. The
native stream cannot move to a later default source; the actual selected
source index remains checked.

The predicate must establish explicit talk enable, the client's OS permission,
and the original controlling session's authority. The adapter does not create
those permissions. PulseAudio must independently authorize the actual record
stream. A successful connection to a daemon alone does not establish recording
readiness, physical microphone qualification, or an OS consent prompt.

Permission is checked before setup, immediately before RECORD begins, around
native event-loop and read calls, before and after every encode, and before
an encoded batch is consumed. Predicates must be nonblocking. A failed or
panicking predicate terminally fences the original owner and its retained
batches. `Control::stop` can fence it from another thread; it neither keeps the
connection alive nor affects a replacement owner. Closing stops this recording
stream, without removing or muting the selected device.

The source and native codec remain confined to the creating worker thread.
Use the existing process-supervision discipline: native setup/destruction and
codec calls are outside the authority path, and a foreign stall still needs
external supervision. This API does not claim an OS sandbox or introduce an
async runtime. A fresh source requires explicit permission again; no reconnect
or generation-reset operation automatically resumes recording.

## Audio and freshness bounds

Capture uses 48-kHz native-endian S16 mono/stereo. PulseAudio converts the
selected source's actual format at this boundary; the adapter does not assume
physical devices are natively 48 kHz. Opus frames are 10 or 20 ms, with the
caller-selected bitrate and payload bound checked before native setup.

The existing shared record/encode implementation retains:

- At most 100 ms in the negotiated native record queue, checked against the
  server's actual response.
- One partial PCM frame and four fixed complete-frame scratch slots.
- At most four encoded packets per pull.
- One original pull expiry, at most 100 ms ahead. A partial frame keeps its
  first expiry across pulls; combining it with a later pull cannot renew it.

A batch carries its original expiry. The caller reads `Batch::expires`, then
calls `Batch::consume` with the original permission check and preserves that
expiry through worker IPC and the eventual send boundary. Consumption after
expiry or terminal stop refuses; a new request cannot renew an old batch.
These are local custody bounds, not a physical-capture latency measurement.

The receiver's jitter epoch fixes sequence-to-sample spacing. Capture holes,
raw-frame eviction, or possible native overruns therefore return
`CaptureDiscontinuity` and retire the microphone source **before encoding**.
The adapter never rewrites timestamps or emits a hidden sample jump with the
next consecutive packet sequence. Emitted packets additionally pass exact
cross-batch direction, generation, duration, sequence and sample-time checks.
Restarting requires a fresh explicitly admitted generation. Host playback's
existing source API and gap-accounting behavior remain separate.

## Executed native evidence

On October 31, 2026, the repository-owned `pulse_microphone` test executed
against the actual `fr-native` package, a private PulseAudio 16.1 daemon from
signed Ubuntu Noble packages, and verified libopus 1.5.2. A separate Python
process supplied a 1-kHz synthetic signal to a test-owned `module-pipe-source`.
The real capture/Opus path produced 86 packets; native decoding found the
injected tone in all 86. No physical microphone or user's audio daemon was
selected. This demonstrates source-to-Opus flow, not hardware qualification.

Four cases cover explicit-selection/consent refusal, real sound and fixed
packet timelines, terminal stop and retained-batch fencing, permission loss,
original expiry, caught permission panics, capture-pressure discontinuity,
replacement-owner isolation, and daemon failure. An independent `pactl`
inventory additionally checks recording-stream removal while the selected
source remains present. The two existing native playback-capture tests also
passed against the refactored shared implementation.

The tests are exercised using an external manifest referencing the actual
package and checked-in test files, avoiding unrelated heavyweight development
dependencies in the local memory-limited environment. No source implementation
was copied or replaced. The ordinary repository commands are:

```sh
cargo check -p fr-native --no-default-features \
  --features linux-microphone-capture --lib --locked
cargo test -p fr-native --no-default-features \
  --features linux-microphone-capture --test pulse_microphone --locked -- \
  --include-ignored --test-threads=1 --nocapture
```

Native test dependencies are PulseAudio and its standard modules, `pactl`,
Python 3, and libopus 1.5.2. The decoder deliberately rejects the system's
older libopus 1.4 because it lacks the required decoder-complexity control;
this check was preserved. Tests using intentional permission panics print
the normal panic-hook message, then assert terminal denial and continue.

Transport attachment, host approval and visible client/host talk indicators,
physical microphone consent, hardware device-change behavior, and end-to-end
latency remain separate integration and qualification work. The host output
boundary is described in [VIRTUAL_MIC.md](VIRTUAL_MIC.md); the codec contract
is in [NATIVE_OPUS.md](NATIVE_OPUS.md).
