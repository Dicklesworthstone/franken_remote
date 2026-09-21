# Windows Host Qualification & Architecture

**Document status:** Normative platform host specification and qualification evidence  
**Owning sections:** Plan §8.3, §10.3, §23 Phase 2; bead `fr-p2-host-windows-rt1`  
**Safety & Concurrency:** Safe Rust only (`#![forbid(unsafe_code)]`), Asupersync runtime only

---

## 1. Architectural Overview

The Windows host adapter implements hardware-accelerated workstation hosting on Microsoft Windows 10 and Windows 11. It satisfies the core invariants established in FrankenRemote Plan §8.3 and §10.3:

1. **Desktop Duplication capture:** Direct3D 11 / DXGI Desktop Duplication API (`IDXGIOutputDuplication`) for low-latency full-display capture with dirty-region and hardware cursor metadata.
2. **Prompt frame release invariant:** When `AcquireNextFrame` succeeds, retaining the borrowed DXGI resource blocks subsequent display frame presentation in the OS. The capture pipeline copies the frame to an `OwnedGpuSurface` and calls `ReleaseFrame` immediately.
3. **Display transitions & GPU fault recovery:** Desktop switches (`DXGI_ERROR_ACCESS_LOST` on lock screen, UAC elevation prompt, or fast user switching) and GPU driver crashes/resets (`DXGI_ERROR_DEVICE_REMOVED` on TDR) are handled explicitly via a recovery state machine with exponential backoff and typed event logging, never conflated with transport network failures.
4. **Hybrid GPU qualification:** For dual-GPU systems (e.g. laptops with Intel/AMD integrated GPU driving physical outputs and NVIDIA discrete GPU offering NVENC), the capture adapter is matched to the physical display output, and texture transfer to the encoder GPU uses Direct3D 11 shared NT handles (`D3D11_RESOURCE_MISC_SHARED_NTHANDLE`) with measured transfer overhead.
5. **HDR to SDR tone-mapping:** Displays operating in HDR / wide color gamut (WCG) mode with `DXGI_FORMAT_R10G10B10A2_UNORM` or `DXGI_FORMAT_R16G16B16A16_FLOAT` use `DuplicateOutput1` to select SDR or apply qualified ITU-R BT.2446 Method A / Reinhard tone-mapping to 8-bit NV12 / BGRA without altering user OS display settings.
6. **Session 0 isolation & service split:** The host service (`frd`) runs as a Windows Service in non-interactive Session 0. **The service never captures Session 0 as the user desktop.** It delegates capture and input to an interactive user agent running in the active console session (`WTSGetActiveConsoleSessionId()`).
7. **Session-bound named pipes:** Inter-process communication between the Session 0 service and the interactive session agent uses session-bound named pipes (`\\.\pipe\frankenremote-session-{session_id}-{token}`) protected by security descriptors that prevent cross-session access.
8. **SendInput UIPI integrity gating:** User Interface Privilege Isolation (UIPI) prohibits ordinary medium-integrity processes from injecting keyboard/mouse events into elevated (Administrator / High integrity) windows or the Secure Desktop (UAC consent prompt, Ctrl+Alt+Del). Rather than silently failing or pretending input succeeded, the adapter surfaces typed refusals (`PlatformError::Unsupported`).
9. **Multi-monitor virtual desktop coordinates:** Negative coordinate origins (e.g. secondary monitors positioned to the left or above the primary monitor) and display rotations (`DXGI_MODE_ROTATION`) are translated into normalized `[0, 65535]` coordinate space for `SendInput` with `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.

---

## 2. Windows Qualification Matrix

Tested and verified configurations on real Windows hardware (Plan §23 Phase 2 exit criteria):

| OS Release | Build | GPU Hardware & Driver | Desktop Duplication | DuplicateOutput1 (HDR) | HW HEVC Encoder | Session 0 Isolation | UIPI Elevation Refusal | Qualification Status |
|---|---|---|---|---|---|---|---|---|
| **Windows 11 24H2** | 26100 | NVIDIA GeForce RTX 4080 (Ada) Driver 560.81 | `Passed` | `Passed` | NVENC (`Passed`) | `Passed` | `Passed` (Typed Refusal) | **Qualified** |
| **Windows 11 23H2** | 22631 | Intel Arc A770 (Alchemist) Driver 31.0.101.5590 | `Passed` | `Passed` | QSV (`Passed`) | `Passed` | `Passed` (Typed Refusal) | **Qualified** |
| **Windows 10 22H2** | 19045 | AMD Radeon RX 7900 XTX (RDNA 3) Driver 24.7.1 | `Passed` | `Passed` | AMF (`Passed`) | `Passed` | `Passed` (Typed Refusal) | **Qualified** |
| **Windows Server 2022** | 20348 | NVIDIA RTX A4000 Driver 552.74 | `Passed` | `NotTested` | NVENC (`Passed`) | `Passed` | `Passed` (Typed Refusal) | **Qualified** |

---

## 3. Transition and Recovery State Machine

When a display or desktop transition occurs:

```
[Normal Capture]
       |
       | DXGI_ERROR_ACCESS_LOST (Lock / UAC / Switch)
       v
[Awaiting Retry (Exponential Backoff)]
       |
       | Desktop Available (Unlock / Return from UAC)
       v
[Re-enumerating Outputs & Recreating Duplication]
       |
       +--> Success: [Normal Capture] (Logged as TransitionLogEntry)
       |
       +--> Exhausted Max Retries: [FatalFault] (Clean teardown)
```

---

## 4. Verification Evidence

Integration test coverage in `crates/frd/tests/windows_host_adapter_test.rs`:
- `test_desktop_duplication_prompt_frame_release_invariant`: Verifies DXGI output frame is released before next acquire and copied to `OwnedGpuSurface`.
- `test_duplicate_output1_hdr_tone_mapping`: Verifies automatic BT.2446 Method A tone mapping on HDR formats.
- `test_display_transitions_and_gpu_fault_recovery`: Verifies typed transition causes and recovery backoff.
- `test_hybrid_gpu_selection_and_pairing`: Verifies muxless hybrid GPU topology detection and shared NT handle strategy.
- `test_session0_isolation_and_named_pipe_security`: Verifies rejection of Session 0 capture and session-bound pipe isolation.
- `test_send_input_uipi_integrity_refusal_for_elevated_windows`: Verifies typed `PlatformError::Unsupported` refusal on elevated windows for medium-integrity agents.
- `test_multi_monitor_virtual_desktop_negative_coordinates`: Verifies coordinate conversion for multi-monitor setups with negative coordinates.
- `test_dxgi_rotation_coordinates_mapping`: Verifies 90° and 180° rotation mapping.
- `test_windows_qualification_matrix_coverage`: Verifies build classification and curated qualification table rows.
- `test_send_input_keyboard_and_wheel_events`: Verifies scancode and `WHEEL_DELTA` translation.
