# Clock-paced audio playout

`fr_client::audio::playout::AudioPlayout` joins the negotiated jitter buffer,
existing `AudioDecoder` contract, local volume/mute and one immediate output
submission. It owns one decoder privately. It adds no codec, runtime, network
channel or native device, and must run on the containing audio worker rather
than the realtime callback or input-authority thread.

Construct it only for an already admitted, enabled and acknowledged audio
configuration. Supply the client output device's consumed sample position in
48 kHz per-channel units plus a fresh `ClientInstant`. Host source timestamps,
client monotonic time and the device sample counter have independent origins;
the owner never compares their raw values. A device change or suspend retires
the stream and requires a strictly newer generation and a new decoder.

`receive` records the first arrival's absolute 100 ms expiry. Duplicate packets
cannot refresh it or move the output schedule. Startup eviction cannot refresh
the original startup deadline. The bounded arrival metadata mirrors the existing
16-slot jitter queue; expired queued audio is terminal even when newer packets
continue arriving. A 100 ms prebuffer target is refused because it leaves no
room before expiry; impossible negotiated jitter configurations also refuse.

`render` performs at most one real decode or one explicit negotiated-size PLC.
The device sample counter, not packet arrival or polling frequency, controls
progress. Repeated calls at the same counter do not replay sound or consume PLC.
Missing a complete device-frame slot stops rather than dumping a backlog into
the speaker. Concealment retains the jitter owner's finite 100 ms loss bound.

A caller-owned checkpoint verifies the current direction/epoch permission and
returns fresh clocks before decode and again before output. Decoder latency
cannot renew a packet deadline. Output receives borrowed PCM, its original
source identity, explicit concealment metadata and both local clock bounds.
The submission adapter must recheck permission and those bounds at its actual
OS boundary, be nonblocking and retain no unbounded queue. A receipt says
submitted, not audibly observed. Video never participates in this service loop.

There is no decoded FIFO. Wrong decoder generation, channels, sample rate,
duration or timestamp refuses. PCM storage is cleared on return/unwind; the
native decoder and encoded queue retire on fatal error, output refusal or caught
panic. A failed reconfiguration cannot revive either the old or attempted epoch.
Direction cannot change during reconfiguration: downlink approval never enables
a microphone. Stop is idempotent. The outer owner must also stop/flush the actual
device and supervise an unresponsive foreign decoder; callbacks are not a
replacement for those platform/session boundaries.

## Native wire-to-Opus receive boundary

Enable `fr-native`'s opt-in `linux-audio-playout` feature to use
`fr_native::opus::playout::OpusPlayout`. It joins complete FRD0 records to the
existing real libopus decoder and this paced owner. It depends on `linux-opus`
and `fr-client`, not the CLI, HEVC, an async runtime or an audio-device library.

Construct the owner only after session admission, explicit direction enable,
configuration acknowledgement and device setup have succeeded. One nonzero
channel binding, direction, epoch and negotiated configuration remain fixed for
its lifetime. Cross-field decoded-sample limits are checked before native
allocation. Narrowed packet-byte and decoded-sample limits apply before copying
wire payload into the jitter queue and again at the actual native decoder.

A matching bound `AudioStop` immediately retires the decoder and queue. Wrong
bindings refuse; other directions/epochs are ignored. Remote configuration and
acknowledgement records cannot reopen this owner or grant microphone permission.
The containing session must enforce epoch advancement when constructing the next
owner; a standalone constructor is not a persistent anti-replay registry. Clock,
permission, submission and native-device cleanup duties above still apply.

## Executed scope

The final four-crate command passed **832 tests, zero failed or ignored**, including
all 14 jitter integration tests, all 15 new playout-owner tests and the existing
compile-fail doctests:

```sh
cargo test --offline --locked --all-features -p fr-core -p fr-wire -p fr-media -p fr-client
cargo clippy --offline --locked -p fr-client --all-targets --all-features -- -D warnings
cargo clippy --offline --locked -p fr-native --no-default-features --features linux-audio-playout --lib -- -D warnings
```

The jitter tests include all 256 eight-packet loss patterns. The owner tests use
an explicitly labeled probe decoder for clocks, expiry, stalled/flooded startup,
revocation before/during decode, incorrect PCM, output refusal, panic cleanup,
local mute, reset failure and both directions across admitted durations. These
probe tests are not codec evidence. This combined final run supersedes the
previous 806-test checkpoint whose restored checkout omitted the 14 separately
verified jitter tests; the added all-feature cases are included in 832.

Six additional native composition tests pass against actual system libopus 1.5.2:
real encode -> FRD0 serialization -> bound receive -> jitter -> clock-paced native
decode -> borrowed PCM submission. They exercise all 20 native encoder profiles
(5/10/20/40/60 ms, mono/stereo, both directions), reordered/duplicate/lost packets,
real PLC, malformed Opus, resource limits, matching/stale stop, permission loss
and fresh-generation native history. Exact output is compared with an
independently driven instance of the same native decoder: an ordering/history
oracle, not an independent codec implementation. No audio samples are logged.

All six also pass Rust AddressSanitizer with leak detection. The system libopus
binary is not sanitizer-instrumented; this is not complete codec memory-safety
certification. Native production and the exact integration-test target pass
strict pedantic Clippy; changed-file formatting and whitespace checks pass.

Every relevant first-party dependency is source-built with nightly-2026-08-31,
using the checksum-verified 52ed96a source/vendor artifact and exact later
shared/native source blobs checked against their committed Git object hashes.
The native test command uses an external Cargo manifest pointing to the actual
checked-in `crates/fr-native/tests/opus_playout.rs` and production dependency
paths, avoiding unrelated CLI/daemon development dependencies. It substitutes no
codec or first-party implementation. Production checks use the actual workspace
manifest. The six tests can also be selected through the normal native package
with `--no-default-features --features linux-audio-playout --test opus_playout`;
that full development-dependency build was not executed here.

These are source/policy and real-codec composition checks, not full-workspace,
physical-device, audible-latency, live-transport or hardware qualification.
Per-OS capture/playback, supervised audio-worker/session integration, device queue
retirement, drift correction and cross-host A/V alignment remain open under
fr-p2-audio-playback-lel and plan 15.4. See [native Opus](NATIVE_OPUS.md).
