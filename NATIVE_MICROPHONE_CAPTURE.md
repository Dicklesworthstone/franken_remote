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
The `linux-microphone-process` feature supplies the actual supervised process
owner described below. The low-level source API itself must not run on an
authority or UI thread. Neither boundary claims an OS sandbox or introduces an
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

## Supervised capture and controller-bound transmission

`fr_native::audio_process::MicrophoneCapture` launches the installed
`fr-audio-worker` with one explicit source, one immutable generation, and the
original authorization deadline. Its supervisor owns spawning, bounded socket
IPC, native hangs and actual reaping. The caller never waits for capture,
encoding, IPC or child destruction. The supervisor retains a nonblocking
permission monitor and checks it during exchanges and idle turns, independently
of application polling. Revocation closes a dedicated terminal channel before
kill/reap. The child checks that channel and the original same-boot monotonic
deadline around native work. Parent-death binding is installed before capture.

There is one command/reply slot, at most four packets per batch, and at most
four capture workers including cancelled children awaiting reaping. An
uncollected reply expires at its original deadline. Absolute clock conversion
can only shorten the deadline; IPC, delayed polling and partial PCM cannot
restart it. `Retirement::is_complete` separately reports actual reaping. The
worker clears its environment and cannot acquire a network connection or input
grant from this protocol. This provides process containment, not a same-user
kernel sandbox.

`audio_process::transmit::Transmitter` composes that process with the new
`fr_client::audio::uplink::Sender`. The sender is attached only through a real
accepted `InputClient` or `PresentedInput`, the original parent/channel scope,
positive `native-audio-up` selection, and independent host microphone approval.
Its separate authority projection follows the original controller's lease,
clock, view readiness and lifetime; microphone failure does not revoke input,
clipboard or files.

An OS grant alone leaves the sender muted. A fresh explicit talk action creates
a strictly newer audio generation. The exact positive `AudioConfigured` reply
is required before the transmitter can launch a recorder. Mute, device change,
unknown permission, controller loss or source failure fences the original
permit and pending speech. Restoring permission never resumes an old talk.
`State` reports the current local capture status for a containing UI; it does
not claim that a remote application heard sound.

Each capture receipt starts before the native pull and keeps its original
100-ms maximum custody. The transmitter retains at most one four-packet batch
and one framed record, admits at most four records in a service turn, and
stops pulling when transport is blocked. Sequence, generation, direction,
duration and contiguous source timeline must match before wire admission.
The containing authenticated transport must preserve the supplied deadline and
original permission through its own retained send queue. `Transmitter::egress`
provides a read-only guard for that exact talk epoch; mute, permission loss,
source failure and owner destruction close every retained clone. A new talk
cannot launch a replacement until the previous child is positively reaped, so
the original cleanup receipt is never overwritten while work is still alive.

## Continuous talk across original-controller renewals

`Sender::refresh_capture` may issue a successor `CapturePermit` only within the
same still-live talk epoch, after fresh accepted authority from its original
controller and before the currently installed cap expires. It cannot adopt a
replacement controller with equal numeric IDs. Each permit remains immutable;
expiration of a superseded permit refuses that item without revoking a valid
successor. Expiration of the current epoch cap remains terminal.

`CaptureMonitor` is a read-only original-owner projection for the independently
serviced supervisor. It observes explicitly installed sender refreshes, but it
cannot mint a permit or renew a grant. A retained packet still needs its own
fixed capture receipt and send deadline.

The same child accepts a bounded `RENEW` command through its existing one-slot
mailbox. Its serial and generation must match, and the exact acknowledgement
must complete before both the old authorization cap and the fixed 100 ms
exchange deadline. The parent installs the successor only after that reply and
a fresh original-owner permission check. A partial, frozen, late or uncollected
acknowledgement cannot extend the exchange. The child independently fences at
its installed same-boot deadline; neither side can revive an expired owner.

`Transmitter` renews only at a drained pull boundary and yields both when staging
the exchange and after collecting its acknowledgement. Only subsequent turns
may capture or admit speech under the successor. Existing partial PCM, captured
batches, wire records and escaped `Egress` guards keep their original caps.
The containing transport snapshots `egress()` before each service turn and
retains that exact guard with any record admitted during that turn.

Extensions occur only within the final second of the current cap, require at
least 100 ms of useful advancement, and are limited to four exchanges per second.
A tighter authority cap is applied immediately. Renewal preserves the selected
source, original child PID, audio generation and contiguous source timeline;
it never implies new consent, source replacement or automatic recording restart.

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

The checked-in `microphone_process` and `microphone_transmit` suites also ran
against the real package and standalone child, with all native cases explicitly
enabled. Process-owned capture produced 39 packets, all matching the independent
1-kHz fixture. The controller-bound transmitter produced 35 wire records, all
decoding to that tone. Tests exercised frozen-child expiry, idle revocation,
fixed authorization expiry, idle child crash detection, actual reaping, retained/uncollected result expiry,
local mute, controller destruction without application polling, and late
transport capacity. Independent `pactl` inventory confirmed recording stopped.
Private IPC/clock tests and strict library/harness Clippy passed. Grant and
presentation inputs in the transmitter test are explicitly synthetic policy
fixtures; these results do not claim an authenticated network session.

```sh
cargo test -p fr-native --no-default-features \
  --features linux-microphone-process \
  --test microphone_process --test microphone_transmit --locked -- \
  --include-ignored --test-threads=1 --nocapture
```

The cleared-environment worker must resolve qualified libraries from its
installed image. The isolated local test build uses a verified, test-only
RPATH for PulseAudio/libopus; it is not a production installation policy.

The subsequent continuous-talk extension passed all 45 focused tests: 20 client
uplink tests, eight private protocol/clock tests, ten worker-process tests and
seven transmitter tests. Strict client/native library and focused harness
Clippy passed. The actual unchanged capture child produced 61 packets, including
13 after its original cap. A 4.3-second controller-renewal run produced 209 real
wire Opus packets, all decoding as the independently supplied tone, including
58 after the original three-second cap. PID, generation, sequence and sample
timeline stayed continuous; the old `Egress` remained expired while the refreshed
guard stayed live. Mute then retired and actually reaped the child. Frozen
renewal and uncollected-acknowledgement cases independently verified terminal
expiry without installing another lifetime.

These are actual native capture/codec results composed with synthetic controller
and presentation policy fixtures. They do not establish uninterrupted talk over
an authenticated network, physical-device qualification, or user-visible consent.

The wire/transport role and client sender are implemented, but public native
offers remain disabled pending host session integration, separate host approval
and visible client/host talk controls. Physical microphone consent, hardware
device-change behavior, and end-to-end latency remain qualification work. The host output
boundary is described in [VIRTUAL_MIC.md](VIRTUAL_MIC.md); the codec contract
is in [NATIVE_OPUS.md](NATIVE_OPUS.md).
