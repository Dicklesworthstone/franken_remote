# Virtual-microphone endpoint: Phase 0 evidence (open bead `fr-p0-virtual-mic-xyh`)

Updated September 17, 2026. This page records what was actually executed for
the Phase 0 virtual-microphone spike and what remains untested. It follows
[AGENTS.md](AGENTS.md) §7: evidence categories are separate; an untested row
is never "supported with caveats"; this spike does not claim a FrankenRemote
uplink, host OS qualification, or any capability beyond the exact rows below.

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
| Endpoint created and app-selectable as a source node | passed | `pw-cli ls Node`: `node.name = fr-virtual-mic`, `media.class = Audio/Source/Virtual`; ports `capture_MONO`/`input_MONO` |
| Real application recorded from it | passed | `pw-cat --record` (PipeWire 1.6.2) linked `fr-virtual-mic:capture_MONO → fr-recorder:input_MONO`; 192,000 s16 mono 48 kHz samples written |
| Audio flowed end to end | passed | Synthetic 96,000-sample Opus-encoded (10 ms frames) then decoded WAV found byte-exact inside the recording at 1024-sample alignment (`exact_decoded_audio_present: true`) |
| Added latency measured | **not tested** | Alignment offset is not a latency measurement; no round-trip timing was attempted |
| Device-change behavior | **not tested** | Not attempted |
| Desktop session manager (WirePlumber) selection | **not tested** | Server ran without a session manager by design (isolated private bus) |
| pw-cat recorder exit status | 1 (retained) | PipeWire 1.6.2 `pw-cat --sample-count` quits its loop without `drained`; upstream returns 0 only when drained. Upstream sources: pipewire `src/tools/pw-cat.c` `sample_limit` path and final `if (data.drained) exit_code = EXIT_SUCCESS`. Status is evidence, not hidden |
| FrankenRemote Opus uplink into the endpoint | **not tested** | No product audio path exists yet; this was a synthetic file playback |
| macOS CoreAudio plugin row | **not tested** | No macOS endpoint exists in this repository |
| Windows endpoint row | **not tested** | Recommendation below; nothing installed or downloaded |

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

## Open acceptance items (bead remains open)

1. Linux: added latency measurement per endpoint, device-change behavior, and
   a session-manager (WirePlumber) visibility check.
2. Linux: repeat against the real desktop session manager rather than the
   private server, with explicit user-visible device naming.
3. macOS: signed user-space CoreAudio server plugin (BlackHole-style
   architecture), signing/packaging path and TCC interaction evidence.
4. Windows: SysVAD-derived endpoint built, test-signed, installed, selected
   by a real application, with recorded provenance and the recommendation
   above confirmed or refuted by measurement.
5. FrankenRemote: the actual Opus uplink reusing downlink framing/generation
   rules (`fr-p2-audio-microphone-lz6`) — explicitly out of scope here.
