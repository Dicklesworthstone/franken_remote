# Supervised per-epoch Opus decoder

The opt-in `linux-opus-process` and `macos-opus-process` features supply `ProcessDecoder` and the
`fr-opus-worker` executable. It uses the existing Opus codec and AudioPlayout,
not a new runtime, jitter buffer, transport, audio device or codec family.

The client posts at most one configuration/packet/concealment operation and
polls one result. A native supervisor owns process creation, socket I/O and
reaping; the session thread never waits for a child. There are at most four
supervisors per process, including cancelled owners awaiting actual reaping.
Each epoch gets a new child; there is no automatic restart or in-process fallback.
The fixed two-second startup budget and 100 ms native operation watchdog are
measured from submission, not from each read or poll. AudioPlayout independently
retains the earlier original arrival and device-slot deadlines. A response that
misses its original slot is rejected, never replayed or relabelled as fresh.

The child binds parent-task death, clears its inherited environment, and uses
only its parent's connected Unix stream on stdin/stdout. Before configuration
or packet parsing, Linux x86-64 confinement closes other descriptors and installs
`no_new_privs` plus a TSYNC seccomp allowlist. The allowlist admits only IPC on
those descriptors, bounded anonymous non-executable allocation, required runtime
bookkeeping, and exit. It denies file opens, sockets/connections, process or
thread creation, descriptor duplication, and new executable mappings. Address
space is capped at 256 MiB and core/file output at zero. Other architectures or
failed confinement refuse rather than running unrestricted. The executable,
loader, already-loaded native libraries, OS and kernel remain trusted.

The macOS child uses the same original epoch, bounded IPC, watchdog, cancellation
and positively reaped retirement. It verifies the expected parent and the
private stdin/stdout socket peers before reading any codec request, closes
unrelated inherited descriptors, disables core/regular-file dumps and observes
parent exit with an independent kqueue watchdog. It statically links the locally
provisioned codec. This is same-user process crash/hang containment, **not a macOS
OS sandbox or protection against a malicious same-user process**. Linux still
requires its seccomp sandbox; the platform distinction never enables an
unconfined Linux fallback.

The parent validates complete versioned replies, transaction, original epoch,
direction, channel count, timestamp and exact sample/byte count before accepting
PCM. Configuring is not acknowledgement; only a real child configuration reply
can establish decoder readiness. No session credential, input capability, Pulse
or X11 connection reaches the decoder. Output submission remains the containing
playback owner's permission-checked operation and is not proof of audible sound.

Dropping the decoder requests termination and discards mailbox work. The
independent `Retirement` handle reports completion only after the original child
has been reaped. A stuck or uncertain reap retains its admission slot; dropping a
handle does not falsely certify cleanup or allow unbounded replacement threads.
The supervisor sleeps on its condition variable while idle; only an outstanding
native operation uses bounded 10 ms cancellation-check intervals.

## Verification

The implementation's selected test build ran 10 actual-process tests, four
private-IPC codec tests, and all 17 unchanged native encoder/decoder tests:
31 passed, none failed or ignored. Real Opus decode and concealment match the
existing native decoder for mono/stereo and 5/10/20/40/60 ms frames. Tests freeze
or kill the actual child, verify one outstanding operation, original playout
slot rejection, independent watchdog cleanup, bounded admission, and failed
spawn. A separately re-executed sandbox probe demonstrates denial of file opens,
new sockets and processes. The actual worker decodes under the same confinement;
the probe is not evidence of a complete adversarial sandbox audit.

Tests used pinned nightly-2026-08-31, rebuilt first-party source from verified
`bd177a8` plus this slice, and matching retained external CI libraries. Strict
pedantic Clippy passed for the affected library and selected test targets. This
is not a cold dependency build, latest-workspace run, acoustic measurement,
physical audio-device qualification or live-tailnet test. The CLI integration
below selects this decoder instead of its former in-process path.

```sh
cargo build -p fr-native --no-default-features --features linux-opus-process --bin fr-opus-worker
cargo test -p fr-native --no-default-features --features linux-opus-process --test opus_process -- --test-threads=1
```

## Installed client integration

`fr connect --view-only --audio` and `--control --audio` use the process-backed
decoder in their shared Opus playout owner, with PulseAudio on Linux and
CoreAudio on macOS. Build `fr-opus-worker` with `linux-opus-process`/`linux-audio`
or `macos-opus-process`/`macos-audio`, and install the matching executable beside `fr`. A missing
image refuses before dialing as `audio_decoder_unavailable`; confinement/setup
failure is a local audio refusal, never permission to select the old decoder.
Both the original output device and the actual decoder must acknowledge native
configuration before the client emits `AudioConfigured`. The former synchronous
adapter remains for explicitly worker-confined library use and its tests, not
as a CLI fallback. Playback remains independent of remote input permission.
The Mac native volume/mute row updates only the original attempt's local gain;
new attempts preserve the last values in a fresh handle. See
[Mac build, ownership and qualification](docs/macos-audio.md).

The output owner retains decoder retirement separately from output cork/flush.
On reset it stops the old child and refuses a replacement until actual reaping;
no old PCM, decoder history or pending operation crosses the epoch. The existing
output permission, packet bounds, jitter, device clocks and final submission
checks remain in force. A frozen child returns no PCM and cannot hold the CLI.
Mac cleanup additionally retains the original native queue witness, including
partially failed setup. A failed disposal cannot certify retirement or allow a
replacement session to consume that slot.

The complete CLI test binary passed 52 ordinary cases; four additional ignored-by-
default native cases were explicitly executed and passed with the actual private
PulseAudio server, restricted worker and independent monitor. Six unchanged
Opus-playout cases and all 13 existing PulseAudio device/playout cases also passed.
The process target passed both serially and with four test threads; its tests
serialize shared admission-capacity assertions without changing production limits.
Strict pedantic Clippy passed for the integrated native library and complete CLI.

These are scoped codec/device/CLI-owner tests, not a newly executed full remote
workstation or acoustic qualification. Identical source preimages were checked
against main. Rebuilt first-party Rust uses the pinned compiler and matching
retained external libraries; unchanged viewer native archives came from that
same verified source because the local RENDER header is unavailable.
