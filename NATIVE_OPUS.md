# Native Opus codec boundary

`fr-native`'s opt-in `linux-opus` feature implements the existing `AudioEncoder` and
`AudioDecoder` contracts with real libopus, independently of the CLI, HEVC, GUI and async runtime.
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

## Decoder and negotiated resource limits

`opus::Decoder` implements the existing AudioDecoder contract. It parses actual
Opus framing and checks the packet's real sample count, direction, generation,
sequence and non-overlapping source timeline before stateful decode. Nominal
5/10/20/40/60 ms packets and positively configured 80/100/120 ms aggregates are
supported. A duration-bearing header is not sufficient framing validation.
Incomplete framing and lied-about duration cannot resize the decoder buffer or
advance its history. It retains one PCM buffer sized to the configured channel
count and duration (at most 11520 i16 samples / 23040 bytes), plus fixed metadata.
Polling returns the existing bounded AudioPcmFrame type; no codec input is retained.

CodecLimits accepts narrower packet/sample ceilings from already-admitted
negotiation. Impossible values refuse before allocation. Encoder configuration
refuses a nominal quality target which cannot fit that packet ceiling; its native
output limit is the admitted byte limit, not just the absolute maximum. Decoder
configuration checks the sample ceiling before allocating state or PCM. An
oversized packet refuses before parsing or decoding. These immutable resource
limits do not themselves authorize capture, input, playback or a wire binding.

Explicit decode_plc calls run real libopus packet-loss concealment, advancing only
the known missing packet's sequence and sample interval. Concealment cannot start
before a real packet, bypass pending-output backpressure, or exceed the shared
100 ms ceiling consecutively. The next valid packet replenishes the allowance;
refused calls do not advance it. Loss beyond that bound requires re-establishing
the generation rather than replaying indefinitely concealed sound. Explicit
source timestamp gaps remain gaps and do not trigger dummy decode/encoding.

Reset preserves the native allocation but discards buffered sound/history and
requires a strictly newer generation. Stale reset is terminal because the existing
trait has no error return; it cannot relabel old samples as current. Resetting an
unconfigured owner only retires that epoch; subsequent configuration must use a
newer epoch. Shape/duration changes use configure with a fresh generation. Fatal
native errors close the owner and release its storage; preflight failures preserve
its current usable state. Reconfiguration temporarily holds at most two capped
states and two bounded PCM buffers, preserving the old stream on candidate failure.

The decoder explicitly selects complexity zero and does not expose DRED or model
loading. This disables the optional [1.5 neural concealment/enhancement paths](https://opus-codec.org/demo/opus-1.5/).
An older library which refuses that CTL returns UnsupportedFormat, not a silent
fallback. Only Linux with the installed libopus 1.5.2 was exercised here.

## Final executed scope

All 17 new integration tests pass: seven encoder and ten decoder tests. Actual
encode → FRD0 serialization → parse → native decode matches a separate direct-C
libopus decoder oracle. Loss tests explicitly drop packets, invoke real PLC and
resume, comparing exact decoded samples against that oracle. Extended packets are
assembled by the independent native repacketizer, not handwritten mock payloads.
Coverage includes narrowed negotiation limits, forged duration/direction, replay,
malformed framing, overflow, reset/history retirement, reconfiguration, borrowed
buffer lifetime, source gaps and silence. No source audio or PCM is logged.

All 17 also pass with Rust AddressSanitizer and leak detection enabled. The system
libopus binary is not sanitizer-instrumented; allocator interception and the Rust
boundary are, so this is not a complete native codec memory-safety certification.
Strict production and exact test-target pedantic Clippy, changed-file formatting
and whitespace checks pass. The unchanged shared crates pass all 803 tests
(including three compile-fail doctests): fr-core, fr-wire, fr-media and fr-client
with all their features. An initial shared-suite compilation command exceeded the
execution tool's command limit; its bounded complete rerun exited zero. No test
assertions, existing source test bodies, or protocol deadlines were weakened.

Base source is exact 1301a6d4bb7c857c777d6b3b6c1b6884f7044983, archive SHA-256
0019d89981e2b9909f12a64415d6895ca3b01d440d8df6733a447d1af191d458, with the original
Git tree checked before editing. Both native targets use exact checked-in test
files through an external Cargo manifest selecting the production dependency
paths; no retained upstream rlib or alternative implementation is linked. Every
first-party dependency of this slice is source-built. Only the compiler came
from the retained offline toolchain. The existing repository-wide documentation
link failure and unrelated full-workspace/daemon tests are not claimed fixed.
Platform audio devices, supervised audio-worker/session integration, protected
native packaging and audible/latency qualification remain open.
