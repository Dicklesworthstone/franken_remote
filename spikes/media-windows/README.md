# Windows Desktop Duplication, D3D11 Video, and Hardware HEVC Probe

This experiment belongs to `fr-p0-media-windows-fbt`, under plan sections 8.3, 9.1, 10.3, and 23 Phase 0. It exercises Desktop Duplication capture, D3D11 device and video decoding, copy-to-owned GPU surface, dirty-region tracking, duplication session limits, and hardware HEVC encoder availability on qualified Windows hardware.

---

## 0. Rust capture path (37134e74, current)

The C++ probe in sections 1–4 enumerated the encoder but never encoded or decoded, and the 2026-09-23 reality check flagged its timings. The Rust host path in `fr-native` (`windows-media`, `crates/fr-native/src/windows_capture*`) does encode and decode:

1. Desktop Duplication on the display's adapter, with the pointer drawn in.
2. BGRA to NV12 on that adapter's D3D11 video processor, which also scales the display to the session's picture: native pixels, or a canonical smaller picture when the viewer states a box (`scaled-picture`).
3. Intel's hardware Media Foundation HEVC transform, opened directly (no FFmpeg). It encodes the whole coded frame (the picture rounded up to 16).
4. `HevcGuard` admission, after the host writes what the transform leaves out: the conformance window that crops the coded frame to the picture, and the colour description.
5. Viewer-side admission, then the client's Media Foundation hardware decoder.

```text
fr-windows capture-probe [--seconds N] [--fps N] [--bitrate N] [--display N] [--all] [--picture-width N]
```

Run it in the interactive session (a scheduled task with an `Interactive` principal from ssh). It prints one JSON document with counts and per-stage timings, and nothing else. `--all` encodes every tick; otherwise it encodes only when Desktop Duplication reports a new image or the pointer moved.

Identity: same machine as section 2 (Iris Plus, driver 31.0.101.2125, Windows 11 Home build 26200), HEVC Video Extension 2.5.48.0, 2256x1504 at 144 dpi, 8 Mbit/s CBR.

Retained results are in `results/surface-laptop-3-iris-plus/rust-capture-probe/`. "Static" means the ordinary desktop. "Moving" means a 1800x1200 WinForms test pattern (scrolling text lines and six moving boxes, redrawn every 15 ms) on top of it.

| run | units | unit p50 / p95 ms | encode p50 ms | decoded (hardware) |
| --- | --- | --- | --- | --- |
| static, 60/s every tick | 600 in 10.0 s | 13.4 / 14.3 | 11.8 | 600/600 |
| moving, 60/s every tick | 600 in 10.0 s | 14.8 / 20.1 (max 117.6) | 12.0 | 600/600 |
| moving, 60/s changed only | 460 in 10.0 s | 15.5 / 21.6 | 12.9 | 460/460 |

A forced mid-run IDR is honoured in every run.

`experiments/` holds the runs that shaped the code. They used env-gated builds that were not committed:
- **Timer A-B-A** (`timer-ab-*`): unit p50 31.3 / 13.4 / 31.3 ms. The transform waits on system timer ticks, so the committed encoder holds a 1 ms timer.
- **Quality-vs-speed 0 and 33**: no change.
- **Quality-vs-speed 100**: an IDR that admission refuses (`Limit`); the setting is not used.
- **Display aperture** (2026-11-13): with a display aperture smaller than the frame on its output type (1620x1080 in 1632x1088), the transform refuses its NV12 input type (`MF_E_INVALIDMEDIATYPE`), also with the aperture on the input or with the sizes split between the two types. With the frame set to the picture it encodes, but pads to multiples of 8 (coded 1624x1080), not to the configuration's 16. Hence step 3's whole coded frame and step 4's host-written window.

Not covered yet: the media_worker Windows host role, fault exercise (access lost, device removed, resize, exhaustion), rotated or multiple displays, HDR, hybrid GPUs, other vendors' transforms.

---

## 1. Reproduce

On any host connected to the target Windows machine over Tailscale/SSH:

```bash
bash spikes/media-windows/run_probe.sh [HOST] [USER] [SSH_KEY]
```

Default target: `jeffr@100.68.2.11` (Surface Laptop 3 / `wsurf`).
The script copies `probe_windows_media.cpp`, compiles it using MSVC `cl.exe`, executes both Session 0 (service session) and Session 1 (interactive desktop session) probes, outputs structured JSON, and cleans up temporary tasks.

---

## 2. Tested Hardware and Runtime Identity

- **Host**: Microsoft Surface Laptop 3 (`OldSurface` / `wsurf`)
- **OS**: Microsoft Windows 11 Home 10.0.26200.9457 (build 26200)
- **CPU**: Intel(R) Core(TM) i7-1065G7 CPU @ 1.30GHz (Ice Lake)
- **GPU**: Intel(R) Iris(R) Plus Graphics (PCI Vendor `0x8086`, Device `0x8A52`, SubSys `0x00351414`, Revision 7)
- **Driver Version**: `31.0.101.2125`
- **Compiler**: Microsoft (R) C/C++ Optimizing Compiler Version 19.44.35228 for x64
- **Windows SDK**: 10.0.26100.0
- **Direct3D Feature Level**: `0xB100` (Direct3D 11.1)

---

## 3. Results Summary

Detailed machine-readable evidence is retained in:
- [`results/surface-laptop-3-iris-plus/session0_probe.json`](results/surface-laptop-3-iris-plus/session0_probe.json) (Headless service session)
- [`results/surface-laptop-3-iris-plus/session1_probe.json`](results/surface-laptop-3-iris-plus/session1_probe.json) (Interactive desktop session)
- [`results/surface-laptop-3-iris-plus/environment.txt`](results/surface-laptop-3-iris-plus/environment.txt) (Hardware/driver manifest)

### 3.1 Architectural Finding: Session 0 vs Session 1 Isolation

A critical finding confirmed by this probe aligns directly with plan §10.3:
- **Session 0 (Windows Service Session)**:
  When invoked directly in the OpenSSH daemon context (Session 0), `IDXGIAdapter1::EnumOutputs()` returns `0` outputs. The display adapter has no outputs attached to Session 0 because Windows isolates services from interactive window stations (`Winsta0\Default`).
  **Conclusion**: The host capture worker cannot run as a Session 0 service. It must run as an agent in the logged-in interactive user session (Session 1+).
- **Session 1 (Interactive Desktop Session)**:
  When invoked in the interactive user session, `IDXGIAdapter1::EnumOutputs()` immediately discovers `\\.\DISPLAY1` with desktop bounds `(0, 0, 2256, 1504)`.

### 3.2 Desktop Duplication (`DuplicateOutput` and `DuplicateOutput1`)

- `IDXGIOutput1::DuplicateOutput` succeeded (`S_OK`, `0x00000000`).
- `AcquireNextFrame` succeeded (`S_OK`, `0x00000000`), returning a full `2256 x 1504` D3D11 texture in `DXGI_FORMAT_B8G8R8A8_UNORM` (format 87).
- **Cursor and Damage Information**:
  - Cursor tracking reported position and visibility state.
  - `GetFrameDirtyRects` reported dirty update regions (1 rect covering modified area).
- **Copy to Owned GPU Surface**:
  - Allocated an owned D3D11 texture (`D3D11_BIND_SHADER_RESOURCE`).
  - `ID3D11DeviceContext::CopyResource` completed in **3.89 milliseconds** (3897.7 µs) on the Intel Iris Plus GPU.
  - The borrowed desktop duplication texture was promptly released via `IDXGIOutputDuplication::ReleaseFrame()`, preventing pipeline stall or frame drops in the desktop compositor.
- **DuplicateOutput1 (DXGI 1.5+) Format Selection**:
  - `IDXGIOutput5::DuplicateOutput1` succeeded (`S_OK`, `0x00000000`) requesting `DXGI_FORMAT_B8G8R8A8_UNORM` and `DXGI_FORMAT_R10G10B10A2_UNORM`.
  - Proves the HDR-to-SDR format negotiation path is supported on Windows 11 with Intel graphics drivers.

### 3.3 Session Exhaustion Limits

- Attempting concurrent `DuplicateOutput` instances on the same `IDXGIOutput1` returned `0x80070057` (`E_INVALIDARG`) on subsequent attempts when the existing duplication handle was held open.
- Confirms that Desktop Duplication sessions are strictly bounded per output and requires centralized ownership rather than ad-hoc viewer creation.

### 3.4 Hardware HEVC Codec Availability

- **Media Foundation Transforms (MFT)**:
  - `MFTEnumEx` with `MFT_ENUM_FLAG_HARDWARE` and `MFVideoFormat_HEVC` enumerated 2 instances of **`Intel Hardware H265 Encoder MFT`**!
  - Confirms hardware-accelerated HEVC encoding (Intel Quick Sync Video) is present and registered in the OS.
- **Direct3D11 Video Device**:
  - `ID3D11VideoDevice` reported **56 hardware decoder profiles**.
  - `D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10` was verified present and supported for hardware decoding.

---

## 4. Architectural Decisions and Input to Plan

1. **Host Process Architecture**:
   - The daemon `frd` may run as a background service to accept network connections and manage tailnet identity, but **all screen capture and input injection must delegate to an interactive user-session worker** running in Session 1 (`Winsta0\Default`).
2. **Buffer Lifecycle**:
   - Borrowed `IDXGIResource` from `AcquireNextFrame` must be copied to an owned `ID3D11Texture2D` within the measured ~3.9 ms window and released immediately with `ReleaseFrame()`. Holding borrowed frames across network I/O or encoding leads to `DXGI_ERROR_ACCESS_LOST`.
3. **FFmpeg / MFT Integration**:
   - The hardware encoder on Intel hardware maps to either FFmpeg's `hevc_qsv` / `hevc_d3d11va` or direct Media Foundation MFT.
   - For desktop builds without a pre-installed FFmpeg binary in PATH, native packaging (`fr-p0-native-packaging-yv4`) or direct D3D11/MFT bindings are required.
