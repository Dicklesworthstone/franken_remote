# Native audio inbox overflow

The Linux CLI's session-facing audio output keeps eight fixed-size packet
records for its foreign-work thread. A valid packet arriving while that inbox
is full is discarded before worker custody, rather than terminating the audio
generation. Existing records keep their exact bytes, order and first-arrival
timestamps. There is no replacement queue, blocking send, deadline extension,
codec reset or retry of the discarded record.

The original jitter/playout owner handles the resulting sequence hole through
its existing finite packet-loss concealment. This does not guarantee continued
playback under sustained overload: original packet expiry, missed device slots,
concealment exhaustion, worker stalls and expired permission still stop audio.
A disconnected consumer, malformed record or wrong binding/generation remains
a failure, even at capacity. A matching stop bypasses the inbox immediately;
retirement is still required before a replacement worker may start.

## Completion fields

The shared control/observation completion formatter adds the unsigned,
saturating `audio_output_dropped_packets` field. It counts only validated
records refused because this particular native inbox was full. It is not a
network-loss estimate, transport-level drop count, decoder-concealment count,
PCM-submission count or generation failure. It survives output resets on the
same report. With no audio request it is zero. Text output names a nonzero
count separately from absence, expired/failed generations and actual submitted
PCM; `audibility_proven` remains false.

## Executed scope

On pinned nightly-2026-08-31, 18 bridge tests, three report-lifecycle tests and
two completion-field tests pass. Six new bridge cases cover exact retained
records and arrival times, admission after draining, saturating accounting,
validation and permission checks before drops, disconnected consumers and a
real-libopus burst. The latter encodes nine 10-ms packets, admits eight through
the actual inbox, and conceals exactly the discarded ninth slot. Its PCM
history matches a separately driven libopus decoder without reconfiguration.
This reference is the same codec implementation, not independent interoperability.
Restoring terminal overflow and ignoring consumer disconnection each fails its
regression. Scoped all-target Clippy with warnings denied and formatting pass.

The external source harness compiles the actual CLI audio/threaded modules and
native audio library. Its session boundary is the original output trait copied
into the harness, with minimal CLI container types; clocks, blocked consumers
and PCM submission are explicit fixtures. The exact production completion
function is compiled with its relevant report container, not an entire desktop.
Supporting inputs are checksum-retained c45b547 plus published audio changes;
1,214 supporting core/wire/media/client tests pass. Modified baselines were
checked against current GitHub sources, including concurrent shortcut changes.
Full current-workspace, CLI/frd, PulseAudio-device, authenticated-transport and
live-tailnet qualification were not executed. Existing device-only tests were
not selected. This work does not include the separately unpublished Opus FEC
changes or close the broader fr-p2-audio-playback-lel acceptance bead.
