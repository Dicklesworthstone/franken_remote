# macOS Virtual Microphone Preflight & Capability Evidence

This executable Phase 0 experiment belongs to `fr-p0-virtual-mic-xyh`, under plan
sections 15.4 and 23 Phase 0. It evaluates macOS virtual audio input endpoint
integration via a user-space CoreAudio HAL server plugin (`BlackHole2ch.driver`).

## Scope & Method

1. **Hardware & Operating System Target**:
   - Host: Apple Mac mini (Apple M4 Pro), macOS 26.2 build 25C56.
   - Toolchain: Apple Clang 17.0.0.
   - Installed Plugin: `/Library/Audio/Plug-Ins/HAL/BlackHole2ch.driver` (BlackHole 2ch, version 0.6.1).

2. **Executed Preflight**:
   - `probe_coreaudio.c` compiled with native Apple Clang (`-framework CoreAudio -framework AudioToolbox -framework AVFoundation -framework Foundation`).
   - Validates code signature, certificate authority, team ID, and hardened runtime of the HAL plugin bundle.
   - Enumerate CoreAudio hardware devices, locate virtual microphone endpoint (`BlackHole 2ch`, AudioDeviceID 158).
   - Measures buffer size, sample rate, safety offsets, and driver loopback latency.
   - Attaches `AudioDeviceIOProc` to exercise playback/output into the virtual microphone and capture from the virtual input.
   - Queries `AVCaptureDevice authorizationStatusForMediaType: AVMediaTypeAudio` to verify TCC privacy behavior.

## Retained Evidence & Results

Results are committed in `results/m4-pro-macos-26.2/`:
- `environment.txt`: macOS 26.2 (25C56), Apple M4 Pro, Apple Clang.
- `codesign.txt`: Code signature verification output for `BlackHole2ch.driver`.
- `probe_result.json`: Exact measured properties, latencies, and TCC state.

| Measurement / Property | Measured Value | Analysis |
|---|---|---|
| Device Name | BlackHole 2ch | Virtual HAL server plugin recognized by `coreaudiod` |
| Driver Bundle | `/Library/Audio/Plug-Ins/HAL/BlackHole2ch.driver` | Mach-O universal (`x86_64 arm64`) |
| Code Signature | Developer ID Application: Existential Audio Inc. (Q5C99V536K) | Hardened runtime (`flags=0x10000`), Apple Root CA |
| Sample Rate | 48,000.0 Hz | Exact match for FrankenRemote Opus 48 kHz standard |
| Buffer Frame Size | 512 frames | **10.667 ms** buffer duration |
| Output Latency + Safety | 0 + 0 frames | Zero additional hardware output delay |
| Input Latency + Safety | 0 + 0 frames | Zero additional hardware input delay |
| Total Driver Latency | 512 frames (**10.667 ms**) | Low latency virtual endpoint suitable for live voice uplink |
| Playback without TCC | **Verified (48,000 frames)** | Host daemon can write to driver output stream without TCC permission |
| TCC Privacy Interaction | `NotDetermined` -> Silent Buffers | CoreAudio delivers 0-amplitude buffers to unconsented capture clients |

## Architectural Findings

1. **Signing & Packaging Path**:
   macOS CoreAudio HAL server plugins (`/Library/Audio/Plug-Ins/HAL/*.driver`) require Developer ID Application signing with Hardened Runtime (`flags=0x10000(runtime)`). A signed driver does not require kernel extension approval or test-mode flags.
2. **TCC Privacy Isolation**:
   CoreAudio strictly gates input capture from virtual audio devices under TCC Microphone privacy:
   - When a host application (e.g. video conferencing or meeting recorder) reads from the virtual microphone, macOS prompts the user for Microphone permission for that application.
   - In non-interactive contexts (e.g. SSH daemon) where TCC is `NotDetermined`, CoreAudio runs the stream without crashing but zeroes all input buffers (`max_amplitude: 0.000000`).
   - Conversely, the daemon feeding client uplink audio into the virtual microphone writes to the *output* stream of the device, which requires NO TCC permissions.
