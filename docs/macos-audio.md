# Native Mac client playback

The optional `macos-audio` build connects an explicitly requested host audio
downlink to the Mac client's local default output. It uses the existing native
audio negotiation, original observation permission, Opus decoder process,
bounded worker inbox, jitter buffer and generation-replacement protocol.

This implements the client output part of plan §§15.4, 16.1 and 19.1. The host
must separately offer playback audio. macOS host capture and microphone
forwarding remain separate capabilities and are not enabled by this feature.

## Build and use

Provide the local, trusted Opus SDK's static `libopus.a` explicitly. The build
does not download a codec or choose a runtime library through a search path.
The linked library must satisfy the existing Opus ABI checks (1.5 or newer).

```sh
export FR_NATIVE_OPUS_LIBRARY_DIR=/absolute/path/to/opus/lib
cargo build -p fr-native --features macos-desktop,macos-audio --bin fr --bin fr-opus-worker --locked
```

Install both matching executables together. With a host that has independently
enabled playback sharing:

```sh
fr connect HOST --view-only --audio
fr connect HOST --control --audio --audio-volume 50
```

`--audio-muted` starts the local remote-audio output muted. Gain and mute apply
to this downlink, not to the Mac's system mixer or the host. The initial Mac
implementation selects the current local default output only; Linux-specific
`--audio-server` and `--audio-sink` requests are refused before connection.
A build without `macos-audio` and a missing matching decoder executable also
produce specific preconnection refusals.

When audio is explicitly requested, the viewer includes a separate local audio
row with volume down/up in ten-point steps and Mute/Unmute. The controls retain
the current invocation's preference across reconnects; an old window cannot
change a later attempt. They change future PCM submissions. Already prepared or
queued sound may remain briefly, within the native queue's 40 ms bound; the
controls do not claim to recall sound already submitted to the device.

The current protocol profile cannot fit playback, clipboard and both file
directions together with the Mac's cursor, pixel-scroll, text and input gates.
That combination returns `audio_feature_limit` before host lookup or a native
picker. Playback with clipboard and one file direction is supported, as is
playback with both file directions and `--no-clipboard`. Optional diagnostics
and throughput extensions are omitted first; cursor and input gates stay intact.

## Native ownership and deadlines

The original `NativeObserver` installs the audio output for the requested
viewing or controlling role. Both paths use the same existing session driver
and bounded worker proxy as the Linux client. Neither AppKit's main thread nor
the input-authority path performs Core Audio configuration, PCM submission,
decoder work or native cleanup.

Every audio generation gets a fresh supervised `fr-opus-worker` and a separate
Core Audio queue. The parent validates the original epoch, packet sequence,
format and exact decoded sample/byte count. The Mac child verifies its original
parent and private connected endpoints, closes unrelated inherited descriptors,
disables core/file output and independently watches parent death. This is
same-user crash/hang containment, **not an OS sandbox claim**. Linux retains its
existing mandatory seccomp boundary; no in-process decoder fallback is added.

The native output resolves the default device and pins its UID once. Every
native clock read checks the queue's actual routing, device liveness, sample
rate, buffer size, safety offset, latency and bounded output-stream layout;
a change requires retirement and a fresh negotiated generation. Only 48 kHz
mono/stereo PCM in the negotiated 10 or 20 ms profile is accepted, including
the existing bounded one-sample clock correction.

Three preallocated native buffers bound application PCM storage. Independently,
the complete scheduled sample extent is limited to 40 ms on the original audio
queue timeline, including data the system has copied out of those buffers.
Device buffer, safety and stream latency determine one fixed scheduling lead;
devices that cannot fit that lead and a complete frame inside the bound are
refused. This can include high-latency wireless outputs.

Startup sends one short silent buffer and requires actual forward motion of
the native sample clock before output readiness. A successful API call or
elapsed wall time does not establish that readiness. Queue-clock regression,
discontinuity, loss of a ready device and a stalled sample clock are terminal
for that generation.

The native clock also compares the original interval between Mach's continuous
and awake clocks. A known sleep interval or uncertain clock sampling retires
the generation before another enqueue, independently of AppKit event delivery.
It never adjusts the existing audio timestamps to make old decoded sound fresh.

Immediately before native enqueue, a synchronous callback checks the original
permission and both the frame's wall-clock and sample-slot bounds, after native
preparation and PCM copying. The scheduled end must still precede the original
expiry. Backpressure, delayed work or a decoder reply never refreshes a frame's
lifetime. A mismatched or uncertain actual enqueue is not retried.

Stop fences submissions before immediate per-queue stop/reset and disposal.
The queue's retirement witness becomes complete only after native callback and
queue disposal succeeds, including cleanup after partially failed setup. An
unresolved disposal quarantines its callback storage without retrying an
ambiguous queue handle, and retains one of four process-wide native owner slots;
it cannot create unbounded
replacement queues. The containing worker retains both this witness and the
actual decoder-child reap witness before releasing its own slot or admitting a
replacement.

## Evidence and limits

The implementation reuses the production Opus/ACK/packet/worker pipeline. On the
Linux development runner, the four normal core, wire, media and client test
targets passed 1,709 tests. Sixty existing native Opus/process/playout tests
were exercised using the actual libopus library, original integration test
sources and actual restricted decoder executable. These establish the shared
codec and Linux process behavior; they do not establish Mac native behavior.

Thirty-six tests from the exact common CLI/audio-worker sources also passed in
a focused runner using the unchanged public daemon audio trait declarations,
real native dependencies and real threads. They cover terminal admission,
retirement, final submission counters, deadlines, option combinations and
attempt-local preferences. Strict Clippy passed for those sources on Linux and
the Apple Silicon target. Four real-Pulse-server tests remained ignored. This
runner does not compile the full daemon or exercise a live connection.

The exact new portable output policy passed eight tests for real-clock
readiness, original setup expiry, clock stall/regression/discontinuity, final
wall and sample deadlines, native backlog accounting and arithmetic/precision
bounds. The actual Rust Core Audio wrapper, portable Opus process modules and
shared playout wrapper passed metadata checking and strict all-target Clippy
for the actual Apple Silicon target against the real core/media/client/wire
types. Neither check called a Core Audio API.

The exact C arithmetic for bracketing continuous/awake time was compiled and
executed on Linux, including sleep, regression, uncertainty and overflow cases.
The Mac CI lane also compiles the actual decoder-process boundary with a native
socketpair fixture that checks inherited descriptor closure, wrong-parent
refusal, resource limits and termination of a hung child on original parent
exit. That Darwin fixture has not been executed on this Linux runner.

Apple-SDK compilation, the new Mac parent-watch boundary, actual speaker output,
device switching, sleep/wake, empty-queue clock progression after the silent startup, and
a live remote Mac audio session require the Mac lane and
device qualification. The existing Mac client CI lane now includes the new
feature and both native source boundaries. Full daemon checking on the Linux
development runner was killed while compiling the unchanged pinned Asupersync
dependency, before it reached the new session integration. These rows remain
unverified; successful source checks are not acoustic or platform evidence.

## Native API references

- [AudioQueueGetCurrentTime](https://developer.apple.com/documentation/audiotoolbox/audioqueuegetcurrenttime(_:_:_:_:)) supplies the original queue's sample timeline and discontinuity indication.
- [AudioQueueOutputCallback](https://developer.apple.com/documentation/audiotoolbox/audioqueueoutputcallback) returns reusable storage; it does not establish that the corresponding sound has played.
- [Audio Queue scheduling parameters](https://developer.apple.com/library/archive/qa/qa1636/_index.html) distinguish requested and actual buffer start positions.
