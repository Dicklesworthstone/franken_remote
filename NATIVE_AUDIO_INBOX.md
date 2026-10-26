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


## A replacement waits for the original output to retire

An audio reset fences the original worker immediately, but its native output
and restricted decoder can still need bounded cleanup. A fresh host
configuration arriving during that cleanup previously failed `configure`.
The session counted that refusal as another failed generation and requested
another host epoch. Several fast replies could spend the existing restart
allowance while the first output was still doing legitimate cleanup.

The CLI now retains exactly one validated, strictly newer configuration as
bounded metadata. Session turns wait without blocking, renewing the retired
worker's permission, opening another output or acknowledging readiness. Both
the old thread's completion and its positive decoder/native retirement proof
must hold before the replacement starts. An incomplete or poisoned cleanup
cannot release the process-wide worker slot. A second configuration cannot
replace the pending offer; cancellation also leaves its generation spent.

One absolute two-second setup deadline starts at configuration admission and
covers the retirement wait, replacement startup and configuration reply
admission. Cleanup completion, service turns, worker creation and reply
backpressure cannot extend it. Expiry discards the pending setup or fences the
replacement with `audio_output_setup_timeout`; the existing host/session
restart allowance is unchanged. Host stop, observation loss, reset and drop
fence the pending setup as well as the original worker. Local volume/mute
changes remain visible to a waiting replacement and grant no permission.

The final committed-PCM and failure counters from the old worker are collected
before establishing the new generation's submission baseline. Only an actually
acknowledged replacement with new submitted PCM can clear absence or count as
recovery. Old queued packets and receipts are not replayed into that output.

The focused `output::threaded::tests::replacement` regressions use actual
bounded worker threads and deliberately controlled cleanup, not native device
or audible playback fixtures. They cover waiting and successful replacement,
final old-PCM accounting, the unchanged restart count, pending mute, stale and
competing configurations, poisoned cleanup, original setup expiry, stop and
observation loss, and acknowledgement after expiry. Pinned Rust formatting and
whitespace checks pass. These new Rust tests have not yet executed in the full
CLI: current workspace checking is blocked in the unchanged pinned Asupersync
build by the shared environment's resource limits. The earlier executed-scope
results above do not count as qualification of this new replacement behavior.
