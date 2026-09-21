# Native Opus codec boundary

`fr-native`'s opt-in `linux-opus` feature implements the existing `AudioEncoder`
contract with real libopus, independently of the CLI, HEVC, GUI and async runtime.
It links only the installed Linux `libopus.so.0` ABI. No downloader, alternate
codec, Rust dependency, model loading, dynamic-library path API or shell codec
process is introduced. This is the plan's explicit native libopus exception.

`opus::Encoder` accepts 48 kHz mono/stereo, either direction, and 5/10/20/40/60 ms
frames. Other durations admitted by the broader shared configuration return
UnsupportedFormat, not a silently changed duration. The default is AUDIO mode,
96 kbps, complexity 5; bounded local settings can select 6–192 kbps/complexity 0–10.
There is one pending packet of at most 1275 bytes, no PCM FIFO. Pending output
causes backpressure before entering libopus; rejected shape/generation/timestamp
inputs do not change codec history or sequence. Source sample gaps are preserved,
not filled with invented capture. Sequence/timestamp additions are checked.
Reconfiguration or reopening after close requires a strictly newer generation.

## Native ownership and bounds

The ABI declarations are the minimal stable signatures/constants from Xiph's
[opus.h](https://opus-codec.org/docs/opus_api-1.5/opus_8h.html) and
[opus_defines.h](https://opus-codec.org/docs/opus_api-1.5/opus__defines_8h_source.html).
Native state size is queried and checked against 256 KiB before create. One state
is retained, with at most two during transactional reconfiguration; failed
creation leaves the old stream intact. Exactly one RAII owner destroys each
native state. Owners are neither Send nor Sync; no callback or borrowed input is
retained after a synchronous FFI call. Native error makes that stream terminal.
Output and Debug contain no diagnostic audio samples or packet bytes.

These synchronous adapters belong on a supervised audio-worker thread, not the
input-authority thread or a realtime audio callback. They are not themselves a
process sandbox, session admission, source consent, or a native audio device.
Production worker integration and trusted native packaging are still required.
The system loader/library and native codec remain trust boundaries: this feature
does not claim protected-path packaging or safely load an arbitrary supplied .so.
The shared synthetic codecs remain explicitly test doubles, not this implementation.

## Encoder verification checkpoint

Seven tests pass against actual system libopus 1.5.2 using pinned
nightly-2026-08-31. An independent test-only C decoder consumes every real encoder
packet for both directions, both channel layouts and every admitted duration.
A stereo tone test checks waveform correlation above 0.95 after the codec-reported
delay, not a fabricated constant sample or a byte-identical lossy round trip.
Other tests cover backpressure/invalid-input history preservation against a clean
encoder, timestamp overflow/gaps, generation retirement, silence, bounded quality
choices and repeated allocation/release. These are codec tests, not device,
network, audio latency or hardware qualification.

The first run exposed duplicate destruction from a Rust struct-update expression
copying the native pointer out of a Drop owner. It was fixed by updating the
original unique owner in place; the unchanged native tests then passed. The
failing log is retained. No assertion or native lifetime requirement was weakened.
Tests use an external manifest selecting the repository's exact integration-test
files and production dependency paths, with no synthetic substitutions, to avoid
unrelated native CLI/daemon development dependencies. Production library check and
strict Clippy use the real workspace manifest with --no-default-features and
linux-opus. This is not a full-workspace verification or completion of
fr-p2-audio-playback-lel; per-OS capture/playback and session joins remain open.
