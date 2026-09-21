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

## Executed scope

The 15 deterministic owner tests use an explicitly labeled probe decoder to
exercise pacing, independent clock origins, stale/duplicate/flooded startup,
permission loss before and during decode, late native completion, output refusal,
unwind cleanup, mute, both directions and all admitted durations, reset failure,
clock overflow/regression and drop behavior. They are not codec/device evidence.
All 806 tests in the rebuilt fr-core/fr-wire/fr-media/fr-client source checkout
pass, with strict fr-client all-target Clippy and formatting/whitespace checks.
This run uses the checksum-verified 52ed96a archive with byte-identical upstream
formatting and the committed jitter implementation; the 14 jitter integration
tests passed separately in the preceding commit and are not double-counted here.
No full-workspace, physical-device, audible-latency, transport or hardware
qualification is claimed. Native device and supervised session wiring remain
open under fr-p2-audio-playback-lel and plan 15.4. See [native Opus](NATIVE_OPUS.md).
