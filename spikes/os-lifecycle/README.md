# OS Permission and Lifecycle Discovery Spike (`fr-p0-os-lifecycle-ost`)

This Phase 0 gate spike resolves the operating-system consent, attribution, session-isolation, and lifecycle boundaries for FrankenRemote across Linux, macOS, and Windows. It owns plan sections 5.3, 10.1–10.3, 15.1, and 23 Phase 0.

---

## 1. Executive Summary & Constitutional Principles

A remote workstation daemon cannot erase OS consent boundaries. Discovering permission rules or process-session architectures after building the product is fatal:
1. **Returned Grants Rule**: User consent dialogs and compositor responses return *actual* grants (bitmasks, offered streams, allowed devices). Authority is clamped to what the user and OS actually granted, **never** what the client requested.
2. **Session Agent Boundary**: The interactive-session agent runs in the user's interactive GUI session, owning consent dialogs, visible indicator, input leases, and permission handles. Workers receive narrow delegated capabilities (e.g. a single PipeWire remote file descriptor) and **must never inherit input authority**.
3. **Restore-Token Single-Use Rotation**: Restoration tokens are single-use credentials that must be rotated atomically (`atomic_write` to a temporary file, followed by atomic rename). Stored tokens can be revoked or rejected by the compositor at any time; rejection is a typed refusal (`restore_token_rejected`), not permission to bypass consent.
4. **Lifecycle Authority Fencing**:
   - **Screen Lock / Logout / Fast User Switch**: Immediately revokes input authority and halts observation. Unlocking does *not* silently restore control; re-approval or fresh challenges are required.
   - **System Suspend / Resume**: Suspends active pipelines and increments the host authority generation upon resume, permanently invalidating prior leases and tickets.
5. **No Synthetic Headless Proofs**: If an OS or compositor environment lacks desktop portal frontends, graphical sessions, or TCC capabilities, the result is recorded honestly as `blocked` or `not tested`, never simulated with mock APIs.

---

## 2. Cross-Platform OS Capability Matrix

| Platform / Desktop Environment | Exact Version / Architecture Tested | Capture | Pointer Input | Keyboard Input | Restore Token | Clipboard | Playback Audio | Overall Status & Failure Reasons |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Linux: Headless Server** | Ubuntu 26.04.1 LTS (Linux 7.0.0-30-generic, x86_64, tty session 6444) | blocked | blocked | blocked | blocked | blocked | blocked | **blocked**: `busctl --user` active, but `org.freedesktop.portal.Desktop` unowned; `xdg-desktop-portal` frontend absent; no graphical compositor session. |
| **Linux: Hyprland / wlroots** | Hyprland 0.56.2-1 / Arch Linux Surface kernel 6.19.8, x86_64 | blocked | not tested | not tested | not tested | not tested | not tested | **blocked**: ScreenCast portal v6 active, but VAAPI driver missing (`iHD_drv_video.so`/`i965` absent, FFmpeg exit 251). EIS input unverified. |
| **Linux: GNOME Wayland** | GNOME Shell 46/47 on Wayland | not tested | not tested | not tested | not tested | not tested | not tested | **not tested**: Requires dedicated GNOME Wayland desktop hardware with interactive consent. |
| **Linux: KDE Plasma Wayland** | KDE Plasma 6.x on Wayland | not tested | not tested | not tested | not tested | not tested | not tested | **not tested**: Requires dedicated KDE Plasma Wayland desktop hardware with interactive consent. |
| **macOS: Apple Silicon** | macOS 26.2 (Build 25C56), Darwin 25.2.0, Apple M4 Pro arm64 | blocked (TCC) | blocked (TCC) | blocked (TCC) | not applicable | not tested | not tested | **blocked**: `CGPreflightScreenCaptureAccess()` and `AXIsProcessTrusted()` returned `false` under non-interactive SSH. Codec verified (NAL 20 IDR), but full desktop access requires interactive user consent. |
| **Windows 11 Console Session** | Windows 11 Home 10.0.26200, Intel Iris Plus Graphics | not tested | not tested | not tested | not applicable | not tested | not tested | **not tested**: Reachable via SSH, but RCH worker `wsurf` reported 0 free slots / disk pressure. Desktop Duplication in active console session unverified. |
| **Windows Session 0 (Service)** | Windows Server / Windows 11 Service Context | blocked | blocked | blocked | not applicable | not tested | not tested | **blocked by design**: Session 0 isolation prevents Desktop Duplication and direct interactive `SendInput`. Architecture requires interactive agent split. |

*Note: In accordance with AGENTS.md §7, untested and blocked rows reflect exact environmental realities and missing preflight prerequisites; no synthetic success is claimed.*

---

## 3. Platform Architecture & Protocol Contracts

### 3.1 Linux: Wayland Desktop Portals & PipeWire

The preferred Wayland path uses `org.freedesktop.portal.RemoteDesktop` and `org.freedesktop.portal.ScreenCast`:

```
Client / fr              Session Agent (frd)                  xdg-desktop-portal / Compositor
    |                            |                                          |
    |---- Request Session ------>|                                          |
    |                            |-- CreateSession() ---------------------->|
    |                            |<-- session_handle -----------------------|
    |                            |                                          |
    |                            |-- SelectDevices(types=1|2) ------------->|
    |                            |<-- response -----------------------------|
    |                            |                                          |
    |                            |-- SelectSources(cursor_mode=4, token) -->|
    |                            |<-- response -----------------------------|
    |                            |                                          |
    |                            |-- [BEFORE START] Request Clipboard ----->|
    |                            |                                          |
    |                            |-- Start() ------------------------------>|
    |                            |       [Interactive User Prompt]          |
    |                            |<-- Response(granted_devices, streams, ---|
    |                            |             clipboard, new_restore_token)|
    |                            |                                          |
    |<--- Admitted Grants -------|                                          |
    |     (Clamped to returned)  |-- Atomically persist new restore token --|
    |                            |-- OpenPipeWireRemote() ----------------->|
    |                            |<-- PipeWire FD --------------------------|
    |                            |                                          |
    |                            |-- Delegate PipeWire FD to MediaWorker ---|
    |                            |   (Agent retains Portal session & EIS)   |
```

Key Architectural Invariants:
1. **Clipboard Request Ordering**: Clipboard integration **must be requested before `Start`**. Calling clipboard methods after `Start` is rejected by portal implementations and by FrankenRemote contracts.
2. **Device Clamping**: If the client requested Keyboard + Pointer, but the user only granted Pointer in the portal dialog, `granted_devices = 2`. Any keyboard action submitted will be refused with `typed_refusal: "device_not_granted"`.
3. **Atomic Restore-Token Rotation**: When a replacement restore token is offered in the response dictionary, `RestoreTokenManager` writes it to `.restore_token_<pid>.tmp` and renames it over the target file. The prior token is added to the in-memory consumed set, rejecting any replay attempt (`LifecycleError::StaleRestoreToken`).
4. **Worker Isolation**: The media worker process receives *only* the inherited PipeWire stream file descriptor. It does not possess the D-Bus bus connection, the portal session handle, or the EIS input socket.

### 3.2 macOS: TCC Permissions & launchd Architecture

1. **Process Family Packaging**:
   - `com.frankenremote.frd`: Daemon / listener process.
   - `com.frankenremote.agent`: GUI session agent running inside the active user `aqua` session domain (`gui/<uid>`).
   - `com.frankenremote.worker`: Ephemeral media capture and encode worker.
2. **TCC Entitlements**:
   - `Screen Recording` (`kTCCServiceScreenCapture`): Checked via `CGPreflightScreenCaptureAccess()`. Prompts can only be displayed when requested from an active user GUI context (`CGRequestScreenCaptureAccess()`).
   - `Accessibility` (`kTCCServiceAccessibility`): Checked via `AXIsProcessTrusted()`. Required for `CGEventPost` injection of pointer clicks and physical key transitions.
3. **Idle Sleep Inhibition**:
   - Active viewing/control holds an assertion via `IOPMAssertionCreateWithName(kIOPMAssertionTypePreventUserIdleSystemSleep, kIOPMAssertionLevelOn, CFSTR("FrankenRemote active remote session"), &id)`.
   - The assertion is strictly released immediately upon session teardown, disconnect, or input revoke.
4. **Fast User Switching & Lock**:
   - Monitored via `NSWorkspaceSessionDidResignActiveNotification`.
   - When the session resigns active (user switch or screen lock), input authority is revoked immediately, and observation is blacked out.

### 3.3 Windows: Session 0 Isolation & UIPI

1. **Session 0 Separation**:
   - System services run in Session 0. Since Windows Vista, Session 0 is isolated from interactive desktop graphics (`WinSta0\Default`).
   - Calling `CreateDesktopDuplication` from Session 0 fails (`DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`).
   - The host broker (`frd`) running as a Windows Service must communicate via IPC with an interactive agent running in the active console session (`WTSGetActiveConsoleSessionId()`).
2. **User Interface Privilege Isolation (UIPI)**:
   - `SendInput` calls generated by medium-integrity processes cannot interact with high-integrity windows (e.g. Task Manager, UAC elevation prompts, elevated cmd/PowerShell).
   - FrankenRemote documents this limitation explicitly: ordinary interactive sessions do not bypass UIPI. Bypassing UIPI requires an executable with `uiAccess="true"` in its manifest, installed in `%ProgramFiles%`, and signed by a trusted code-signing certificate.
3. **WASAPI Render-Endpoint Loopback**:
   - Standard WASAPI loopback (`AUDCLNT_STREAMFLAGS_LOOPBACK`) captures all audio rendered to the playback endpoint across the entire system.
   - Per plan §10.3, FrankenRemote discloses this cross-session scope: endpoint loopback is not per-user isolated. Process-specific loopback is supported only on Windows 10 build 2004+ (`AUDIOCLIENT_ACTIVATION_PARAMS`).

---

## 4. Universal Authority Fencing on Lifecycle Events

All FrankenRemote host adapters adhere to the universal lifecycle transition table:

| Lifecycle Event | Observation State | Input Lease State | Authority Generation | Session Validity |
| :--- | :--- | :--- | :--- | :--- |
| **Session Locked** | Blacked out / Paused | **Revoked Immediately** | Unchanged | Valid (requires re-auth on unlock) |
| **Session Unlocked** | Re-verification required | **Remains Revoked** (fresh grant needed) | Unchanged | Valid |
| **User Switch Begun** | Terminated | **Revoked Immediately** | Invalidation fence | **Invalid (Terminated)** |
| **User Logoff** | Terminated | **Revoked Immediately** | Invalidation fence | **Invalid (Terminated)** |
| **System Suspend** | Paused | **Revoked Immediately** | Invalidation fence | Suspended |
| **System Resume** | Re-verification required | **Revoked Immediately** | **Incremented (+1)** | Re-negotiation required |

---

## 5. Verification & Test Execution

The state machine contracts and lifecycle boundary assertions are implemented and tested in `contracts.rs`.

### Running Contract Tests

```bash
rustc --test spikes/os-lifecycle/contracts.rs -o /data/tmp/os_lifecycle_test
/data/tmp/os_lifecycle_test
```

### Test Results Summary

```text
running 6 tests
test tests::test_clipboard_requested_after_start_is_rejected ... ok
test tests::test_user_switch_terminates_session ... ok
test tests::test_system_resume_fences_authority_generation ... ok
test tests::test_valid_portal_sequence_with_grants ... ok
test tests::test_session_lock_revokes_authority_immediately ... ok
test tests::test_restore_token_rotation_and_stale_detection ... ok

test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

All invariants (request-before-start, grants clamping, single-use token rotation, atomic persistence, lock revocation, resume generation fencing, and user-switch termination) passed verification cleanly.
