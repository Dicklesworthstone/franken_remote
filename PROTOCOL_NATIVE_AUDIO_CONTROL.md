# Native playback audio during control

Status: opt-in v0 extension; source implementation, not a hardware or end-to-end qualification report. Implements the playback portion of plan section 15.4. Microphone forwarding is unchanged and is not enabled by this extension.

## Capability and compatibility

The optional capability `native-audio-control` version 1 extends `native-audio-down` version 1 to a `RequestControl` selection. BOTH capabilities must be selected at their exact versions. The legacy `native-audio-down` capability alone retains its observer-only meaning from PROTOCOL.md; no existing record layout or meaning changes.

An `Observe` selection continues to require only `native-audio-down`. A `RequestControl` selection missing either capability continues without audio, without waiting for an audio attachment and without downgrading or refusing control. In particular, older hosts that advertise playback for observers but cannot supply it with control must not cause a new client's startup to hang. Missing optional audio is reported as an absence, not as successful playback.

The host offers the extension only when its operator locally enables both control and playback capture. A client offers it only when the local user explicitly asks for playback during control and the client supplies isolated audio decoding/output. Offering or selecting the extension is not an input grant, local approval, microphone consent, evidence of native readiness, or permission to broaden observation scope.

## Attachment and startup

On a controlled selection with both capabilities, the native bootstrap attaches Configuration, Recovery, Video, Input and AudioDown on the ORIGINAL admitted connection. Every role consumes its own original one-use ticket and binding. The input and audio attachments are distinct; neither attachment grants an input lease. Existing route count, byte, record, binding and generation limits apply unchanged.

The audio channel is joined to the same selected display/view proof and authenticated observation session. It retains the existing AudioDown directions: host reliable AudioConfiguration/AudioStop, viewer reliable AudioConfigured/AudioStop, and host-to-viewer AudioPacket datagrams. There is no uplink, second connection, alternate codec, or independently renewable authority.

Audio capture is demand-driven and uses the existing AudioSource, bounded AudioRing and per-subscription AudioLane. Native capture runs in the supervised audio child, independently polled alongside session service. No packet is sent until that viewer's exact configuration acknowledgement is admitted. Pre-acknowledgement sound is not replayed. Acknowledgements must match the original binding, direction, current epoch and actual negotiated format.

## Authority, scheduling and teardown

Original observation approval and expiry gate audio, including any observation before the local control request. Input still requires its separate explicit grant, presented-view checks, valid tickets and native submission checks. Audio never supplies evidence of screen freshness and cannot renew or resume input authority.

The managed input driver retains poll priority before and after each session turn. Audio source IPC is independently polled; waiting on it never becomes a wait on the input executor. On the client, PulseAudio operations and destruction stay on the bounded foreign-work thread and Opus decoding stays in the restricted per-epoch child. Packet queue time consumes the original receipt deadline; queues are bounded in bytes and count.

On session end, revoke the original authority first, close the audio lane and discard its retained packets, then stop and reap the original child using its retained retirement receipt. No new worker may replace an unreaped one. Source/device failures are typed audio stops, not grants or video freshness, and must not silently create a fresh audio lifetime. Confirmed process exit, transport admission and audible observation remain distinct evidence categories.

## Verification boundary

The source includes capability, route/epoch, acknowledgement, deadline and scheduling regressions. Those tests do not certify speakers, a physical microphone, an installed desktop audio server, live-tailnet interoperability, or audio/input timing under impairment. Bead `fr-rc2-audio-during-control-35e3` remains open until the real controlled-session audio and independent-input acceptance tests pass with retained evidence.

## Experimental Linux CLI path

The executable connection is wired in source by `305c4eb` (host service and
combined offer) and `761faab` (CLI and completion reporting). This supersedes
older descriptions of `--control --audio` as a parser refusal; it is not a
passing build or end-to-end qualification claim.

On the host, explicitly enable both capabilities:

```sh
frd run --software-explicit --input-agent /opt/fr/fr-input-agent --audio
```

On the controlling machine, explicitly request playback:

```sh
fr connect NODE --control --audio --experimental-native --display only
```

`NODE` is the selected installed-tailnet node. Use matching installed binaries;
the client needs `linux-desktop,linux-audio` and its sibling `fr-opus-worker`.
The host needs the existing X11 capture and input workers. Each side can specify
its own local `--audio-server /absolute/path/native` and `--audio-sink NAME`;
these are not peer-selectable paths. Clipboard and explicit `--send` selections
remain independent opt-ins, with their existing host-side enables. No microphone
capability or hardware HEVC qualification is added.

Controlled-session completion now includes the existing playback report:
`audio_requested`, `audio_active`, `audio_frames_submitted`,
`audio_output_resets`, `audio_absence`, and `audibility_proven: false`.
`audio_active` records acknowledged playback with at least one submitted frame
during the attempt, not proof that a device is still playing after closure.
An older host without the extension continues control with typed audio absence.

## Combined-session acceptance tests

`crates/frd/tests/native_host_linux_serial/real_audio/controlled.rs` extends the
existing real-audio fixture, leaving its three observation tests registered.
The new cases require actual host tone detection and independently observed
XTest input; they cover sustained playback across lease renewal, a host without
audio enabled, a scoped SIGSTOP of the client's original Opus child while input
continues, and local host revoke stopping both input and newly introduced sound.
The namespace runner already executes these explicitly ignored tests:

```sh
cargo build -p fr-native --features linux-desktop,linux-displays,linux-input,linux-clipboard,linux-audio
cargo build -p frd --bin frd
cargo test -p frd --test native_host_linux_serial --no-run
FR_NS_SUDO=1 scripts/test_linux_serial_lifecycle.sh /absolute/path/to/native_host_linux_serial-TEST_HASH
```

The final argument is the test executable printed by `cargo test --no-run`.
Run only in the script's isolated namespace. The new Rust tests have not been
executed in the implementation environment. The independent X11 driver scripts
alone passed a local Xvfb smoke check (pointer position and key/button
press/release); that is fixture validation, not FrankenRemote, audio, transport,
physical-device or live-tailnet evidence. Planted-negative runs, the complete
namespace suite and revision-bound acceptance artifacts are still required.
