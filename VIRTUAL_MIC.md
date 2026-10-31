# Virtual-microphone endpoint: Phase 0 evidence (open bead `fr-p0-virtual-mic-xyh`)

Updated October 31, 2026. This page records what was actually executed for
the Phase 0 virtual-microphone spike and what remains untested. It follows
[AGENTS.md](AGENTS.md) §7: evidence categories are separate; an untested row
is never "supported with caveats"; this spike does not claim a FrankenRemote
uplink, host OS qualification, or any capability beyond the exact rows below.

## Linux native endpoint API (implemented; private-server qualification)

`fr-native` now provides an opt-in `linux-pipewire-mic` feature and the safe
`pipewire_mic::Microphone` owner. It creates an actual application-selectable
PipeWire `Audio/Source` with `node.virtual=true`, accepts the existing decoded
`AudioPcmFrame`, and publishes 48-kHz interleaved S16 mono/stereo PCM. This
implements the native endpoint boundary; it does not enable microphone
forwarding in `fr`/`frd`, capture a physical microphone, grant observation, or
advertise a qualified microphone capability.

The caller explicitly selects an absolute local Unix socket. The connected
daemon's peer UID must match the process's effective UID. There is no socket
environment fallback, module installation, autoconnection, default-device
change, persistent source, or network listener. Source names are generated
locally and contain no remote metadata. The owner is deliberately neither
`Send` nor `Sync`: create, poll, and destroy it on one supervised native audio
worker. The only cross-thread handle is the terminal `Control::stop` fence.
The native loop remains entered on that owner thread from creation through
stream/context teardown. This invariant matters: PipeWire 1.0.5 can execute
a queued invocation on its caller's thread when no loop thread is registered,
so omitting `RT_PROCESS` alone does not guarantee callback confinement.

The original approval must be checked when connecting, admitting PCM, and
polling. During `poll`, the borrowed permission check also runs inside the
native callback immediately before submitting PCM to PipeWire. Its lifetime
ends before `poll` returns; no native realtime callback retains it. A failed
or panicking callback closes the original endpoint. These checks must be
nonblocking and avoid allocation or I/O. Driver setup/destruction can still
stall and require worker supervision outside the authority path.

Each endpoint accepts exactly its original generation, channel layout and
10/20-ms frame size. Sample timestamps cannot overlap or replay. A valid
attempt consumes its sample interval even if expired or refused for pressure,
so callers discard that sound instead of retrying with a renewed deadline.
Four fixed input blocks retain their original absolute local deadlines, at
most 100 ms ahead. Native callbacks recheck those deadlines after copying and
the permission check, just before queueing. The bridge allocates no buffers
while processing, uses at most two 4096-frame native buffers, and substitutes
silence for missing/expired/fenced sound. These bounds describe local custody;
already submitted OS audio cannot be recalled and these tests do not establish
an audible end-to-end latency bound.

Typical worker lifecycle:

```rust,ignore
let mut endpoint = Microphone::connect(&selected_socket, original_uplink_config,
    || original_session.microphone_permitted())?;
let stop = endpoint.control(); // supervisor may terminally fence this endpoint
while endpoint.poll(|| original_session.microphone_permitted())? != State::Ready {
    // Yield to the existing worker's bounded supervision; do not busy-spin.
}
endpoint.submit(&decoded_frame, original_frame_deadline,
    || original_session.microphone_permitted())?;
// Continue polling while permitted; on pressure/expiry discard the sound.
endpoint.close();
```

Build dependencies on Debian/Ubuntu are a C compiler, `pkg-config`, and
`libpipewire-0.3-dev` (including its SPA development dependency). The feature
is disabled by default. `--all-features` Linux builds require the SDK. Real
qualification additionally requires `pipewire`, `pipewire-bin`, and Python 3:

```sh
cargo check -p fr-native --no-default-features --features linux-pipewire-mic \
  --lib --locked
cargo test -p fr-native --no-default-features --features linux-pipewire-mic \
  --test pipewire_microphone --locked -- --include-ignored --test-threads=1 --nocapture
```

The repository-owned tests launch a private daemon in a fresh mode-0700
runtime directory. An independent `pw-cat` process records actual PCM; the
helper configures and links only the exact test-owned source/recorder nodes
because this private server has no session manager. It requires matching
synthetic samples, silence after expired sound and terminal stop, and source
removal witnessed through another connection. Additional assertions cover
queue pressure, generation/layout refusals, replay refusal, old stop handles,
daemon failure, and permission loss during the native submission callback.
Missing tools or unsuccessful capture fail explicitly. WAVs and native logs
are retained in the printed test directory; they are synthetic test data.

The corrected Rust API and independent recorder were executed against
PipeWire 1.0.5 with the pinned Rust toolchain on October 31, 2026. The
terminal-stop run recorded 171,008 frames, including 125,440 matching
synthetic samples; the permission-revoked-inside-callback run recorded
162,816 frames, including 122,112 matching samples. Both had zero peak in
the final silent quarter second and server-observed source removal. The
revocation test requires the native callback to execute and queue silence
while the submitted-audio counter remains unchanged. These are sample-flow
and fencing observations, not a latency benchmark.
Real WirePlumber selection, meeting applications, device hotplug, physical
microphone permissions, host/client uplink transport, and end-to-end latency
remain unqualified. The existing abstract `fr-media` endpoint continues to
return typed unsupported until the full session owner is integrated.

The implementation uses PipeWire's documented
[stream source and callback lifecycle](https://docs.pipewire.org/page_streams.html)
and [connected-fd ownership](https://docs.pipewire.org/group__pw__core.html).
The older standalone spike below remains separate historical evidence.

## Linux: private PipeWire endpoint (executed, partial)

Scope: private PipeWire 1.6.2 server from extracted Ubuntu packages
(`pipewire 1.6.2-1ubuntu1.1`, Ubuntu resolute; FFmpeg `8.0.1-3ubuntu2` with
libopus), inside a fresh per-run `XDG_RUNTIME_DIR`. The desktop PulseAudio 17.0
session (`sensedemobox`) was **not** modified: no modules loaded, no default
device changed, no system PipeWire installed. Reproduce with:

```bash
python3 spikes/virtual-mic-linux/probe.py NEW_OUTPUT_DIR \
  --pipewire-root /tmp/fr-virtual-mic.hzK69O/root
# Raw package root is transient evidence; re-download with
# `apt-get download pipewire pipewire-bin libpipewire-0.3-modules` and
# dpkg-deb -x to a fresh root for an independent run.
```

Executed at source commit `ec16268` on 2026-09-17; every step's argv, exit
code, stdout and stderr are retained under the run directory
(`command-*.json`, `server.log`, `recorder.log`, `player.log`, `result.json`).

| Row | Status | Evidence |
|---|---|---|
| Linux: Endpoint created and app-selectable as a source node | passed | `pw-cli ls Node`: `node.name = fr-virtual-mic`, `media.class = Audio/Source/Virtual`; ports `capture_MONO`/`input_MONO` |
| Linux: Real application recorded from it | passed | `pw-cat --record` (PipeWire 1.6.2) linked `fr-virtual-mic:capture_MONO → fr-recorder:input_MONO`; 192,000 s16 mono 48 kHz samples written |
| Linux: Audio flowed end to end | passed | Synthetic 96,000-sample Opus-encoded (10 ms frames) then decoded WAV found byte-exact inside the recording at 1024-sample alignment (`exact_decoded_audio_present: true`) |
| Linux: Added latency measured | passed | PipeWire buffer quantum 480 samples (10.0 ms @ 48 kHz); measured stream buffer delay 256 samples (5.33 ms). Logs in `player.log`/`recorder.log` |
| Linux: Device-change behavior | not tested | Dynamic device-change hotplug not attempted in Phase 0 |
| Linux: Desktop session manager (WirePlumber) selection | not tested | Server ran without a session manager by design (isolated private bus) |
| Linux: pw-cat recorder exit status | 1 (retained) | PipeWire 1.6.2 `pw-cat --sample-count` quits its loop without `drained`; upstream returns 0 only when drained. Status is evidence, not hidden |
| macOS: Signed user-space CoreAudio server plugin | passed | `/Library/Audio/Plug-Ins/HAL/BlackHole2ch.driver` validated: Developer ID Application (Existential Audio Inc. Q5C99V536K), Hardened Runtime (`flags=0x10000`), Apple Root CA |
| macOS: App-selectable virtual endpoint | passed | `BlackHole 2ch` (AudioDeviceID 158), 48 kHz stereo input/output streams, recognized by `coreaudiod` and AVFoundation (`device [1]`) |
| macOS: Driver latency measured | passed | 512 frames buffer size = **10.667 ms** driver loopback latency @ 48 kHz (0 safety offset, 0 device latency) |
| macOS: Playback without TCC | passed | Host daemon writes audio frames to virtual driver output stream via CoreAudio IOProc without requiring TCC permissions |
| macOS: TCC interaction validated | passed | `AVCaptureDevice authorizationStatusForMediaType: AVMediaTypeAudio` returns `NotDetermined` in SSH session; CoreAudio delivers 0-amplitude buffers to unconsented capture clients. Meeting apps require user TCC consent |
| Windows endpoint row | blocked / typed unsupported | Recommendation below (SysVAD / SimpleAudioSample under MS-PL); requires EV code-signing and Microsoft Hardware Dev Center attestation |

## Windows recommendation (recorded for the bead; legal decision stays a release gate)

**Recommendation: derive the Windows endpoint from Microsoft's SysVAD /
SimpleAudioSample virtual-audio-device sample and route real PCM through it,
rather than shipping a renamed third-party virtual cable.**

- Provenance and licensing: the driver samples repository is Microsoft Public
  License (MS-PL) ([LICENSE](https://github.com/microsoft/Windows-driver-samples/blob/main/LICENSE)),
  which permits reproduction, derivative works and distribution with notice
  retention; SysVAD's README documents its architecture and its known HLK
  loopback/offload limitations. This satisfies the bead's "no undisclosed
  third-party driver downloads" constraint **only if** the consumed revision
  is pinned, its hash recorded, and the MS-PL notice retained in the
  distribution record — the packaging bead (`fr-p0-native-packaging-yv4`)
  owns that record.
- Signing path: kernel-mode audio drivers on release Windows require
  Microsoft Hardware Dev Center attestation or full HLK signing with an EV
  code-signing certificate associated with the dashboard account
  ([attestation requirements](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation)).
  A test-signing run (bcdedit /set TESTSIGNING ON) qualifies behavior only;
  it does not validate the shipping signing chain and must be reported as
  such in any capability row.
- Why not VB-Cable-class components: VB-Audio's cable is a commercial
  product requiring purchase and does not offer redistribution terms suitable
  for bundling; per the bead constraint, no undisclosed third-party driver
  download is acceptable even for a spike.
- Remaining prerequisite for any Windows row: interactive Windows 11 machine
  (the configured `wsurf` worker historically inventoried as Windows 11 Home
  build 26200, Intel Iris Plus Graphics) with WDK/EWDK build access, plus the
  test-signing/attestation decision. Those prerequisites are not currently
  verifiable from this host and are **not** claimed.

## Acceptance and Phase 0 Status

1. **Linux**: PipeWire virtual source `fr-virtual-mic` (Audio/Source/Virtual) created; real application recorded 192,000 samples; Opus decoded audio verified byte-exact; buffer latency measured (480 samples = 10 ms quantum, 256 samples = 5.33 ms stream delay). Retained in `spikes/virtual-mic-linux/`.
2. **macOS**: CoreAudio HAL server plugin (`/Library/Audio/Plug-Ins/HAL/BlackHole2ch.driver`) evaluated on Apple Silicon (M4 Pro, macOS 26.2 build 25C56). Code signing validated (Developer ID Application, Hardened Runtime, Apple Root CA). Driver latency measured (512 frames = 10.667 ms @ 48 kHz). CoreAudio TCC Microphone privacy enforcement verified on virtual input (`NotDetermined` -> 0-amplitude buffers). Playback without TCC verified. Retained in `spikes/virtual-mic-macos/`.
3. **Windows**: Recommendation documented to derive endpoint from Microsoft SysVAD / SimpleAudioSample (MS-PL) with recorded provenance and EV code-signing / Microsoft Hardware Dev Center attestation prerequisites. If driver is not bundled/signed, client microphone uplink ships as a typed unsupported capability.
4. **FrankenRemote Product Integration**: The actual bidirectional Opus uplink reusing downlink framing/generation rules belongs to Phase 2 (`fr-p2-audio-microphone-lz6`).
