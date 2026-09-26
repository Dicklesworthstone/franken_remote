# Restricted per-epoch Opus decoder

The opt-in `linux-opus-process` feature supplies `ProcessDecoder` and the
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
PulseAudio device qualification or live-tailnet test. The separate client CLI
integration must select this decoder before its old in-process path is removed.

```sh
cargo build -p fr-native --no-default-features --features linux-opus-process --bin fr-opus-worker
cargo test -p fr-native --no-default-features --features linux-opus-process --test opus_process -- --test-threads=1
```
