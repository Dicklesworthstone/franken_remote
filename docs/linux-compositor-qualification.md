# Linux Compositor Qualification Table and Host Architecture

This document records the Linux platform host adapter design, security invariants, and compositor qualification rows under **FrankenRemote Plan §10.1, §23 (Phase 2), and §24.2**.

---

## 1. Core Architectural Invariants

### 1.1 Session Agent Ownership and Worker Isolation
The Linux hosting architecture splits responsibilities cleanly between the interactive session agent and unprivileged child workers:

- **Session Agent Ownership:** The interactive `SessionAgent` running in the user's graphical session creates and **retains** the `org.freedesktop.portal.RemoteDesktop` session and the EIS (`libei`) input connection.
- **Worker Isolation:** The session agent passes **only** the selected PipeWire stream capability (`PipeWireStreamCapability`) to the on-demand media worker process. A media worker process never receives:
  - The RemoteDesktop portal session handle or D-Bus bus connection;
  - The EIS input socket or input injection authority;
  - Tailscale control sockets, certificate private keys, or approval endpoints.
- If a media worker process crashes, hangs, or is terminated, the session agent retains authority, cleans up held keys, and supervises worker restart without losing session ownership or input fences.

### 1.2 Portal Sequence and Authorization by Returned Grants
Wayland portal session setup enforces a strict protocol sequence:
1. `CreateSession` (session type: `RemoteDesktop`)
2. `SelectDevices` (requested devices: pointer, keyboard, touchscreen)
3. `SelectSources` (source types: monitor/window; cursor mode: hidden, embedded, or metadata; optional restore token)
4. `ConfigureClipboard` (requested **before** `Start`)
5. `Start` (returns granted devices, streams, and replacement restore token)

**Returned grants, not requested flags, define authorization:**
- If the compositor or user grants pointer control but denies keyboard input, keyboard operations are rejected with typed `PlatformError::Unsupported`.
- If clipboard integration is denied or unannounced, clipboard synchronization is disabled.
- The system never infers RemoteDesktop/EIS support from a working ScreenCast portal.

### 1.3 Single-Use Restore Token Persistence and Rotation
Compositors supporting persistent permissions (such as GNOME and KDE) return a `restore_token` for unattended re-connection.
- **Atomic Persistence:** Stored tokens are written to a temporary file, flushed and synced to disk, and atomically renamed to prevent partial writes.
- **Single-Use Rotation:** When a restore token is presented in `SelectSources`, the compositor's `Start` response provides a replacement token (or none). The old token is invalidated and replaced atomically; if no replacement is provided, the old token is deleted.
- **Rejection Recovery:** If a compositor rejects an expired or revoked restore token, the token is consumed, and the host falls back gracefully to an interactive user prompt. It **never** attempts an unauthorized or privileged fallback.

### 1.4 PipeWire Stream Resolution and Node-ID-Reuse Protection
PipeWire numeric node IDs are 32-bit integers recycled by the server when nodes terminate.
- Streams are resolved by the strongest identifier available: prioritizing monotonic 64-bit `pipewire.serial` where exposed by the portal.
- On stream reconnect or renegotiation, stream properties are verified. If a stream presents the same numeric node ID but a different or missing serial, the connection is refused as `NodeIdReused`.

### 1.5 Distinct Coordinate Spaces
Coordinates across the pipeline are kept strictly distinct with checked arithmetic:
1. **Compositor Space:** `CompositorPoint` (floating-point layout coordinates).
2. **Stream Pixels:** `StreamPixelPoint` (raster buffer pixel coordinates).
3. **Crop/Scale Mapping:** `CropScaleMapping` (sub-rectangle and scale factor).
4. **EIS Region Mapping:** `EisRegion` / `EisPoint` (libei region offset, physical dimensions, and device scaling).

### 1.6 Cursor Metadata Gating
- If `CursorMode::Metadata` is granted, out-of-band cursor position and shape buffers are transmitted.
- If `CursorMode::Embedded` is granted, the compositor renders the pointer directly into the video stream; out-of-band cursor metadata is disabled to prevent duplicate cursor rendering.
- If `CursorMode::Hidden` is granted, pointer rendering is omitted.

### 1.7 Security Posture: Wayland vs. X11
The host surfaces its display server security model in runtime diagnostics:
- **Wayland (`WaylandPortalConfined`):** User consent prompts enforced via portals, per-window buffer isolation, no global input snooping, scoped capability delegation.
- **X11 (`X11Unconfined`):** Traditional unconfined trust model; no per-client window isolation (any X client can inspect all windows and snoop global keystrokes/pointer events); `XTest` provides unconfined synthetic injection without portal prompts.

---

## 2. Linux Compositor Qualification Matrix

The table below records independent qualification rows per compositor family with exact software versions, matching Plan §10.1 and acceptance criteria.

| Compositor Family | Tested Environment | Portal Backend | Capture | Pointer | Keyboard | Restore Token | Clipboard | Playback Audio | Overall Status |
|---|---|---|---|---|---|---|---|---|---|
| **GNOME (Mutter)** | GNOME 46.4 / 47.0 (Fedora 40, Arch Linux) | `xdg-desktop-portal-gnome` 46.2 / 47.0 | **PASSED** (PipeWire 1.2.0+) | **PASSED** (EIS) | **PASSED** (EIS) | **PASSED** (Rotated) | **PASSED** | **PASSED** (PipeWire) | **Full Control** |
| **KDE Plasma (KWin)** | Plasma 6.1.5 / 6.2.0 (Fedora 40 KDE, Arch Linux) | `xdg-desktop-portal-kde` 6.1.5 / 6.2.0 | **PASSED** (PipeWire 1.2.0+) | **PASSED** (EIS) | **PASSED** (EIS) | **PASSED** (Rotated) | **PASSED** | **PASSED** (PipeWire) | **Full Control** |
| **Hyprland / wlroots** | Hyprland 0.42.0 (Arch Linux) | `xdg-desktop-portal-hyprland` 1.3.3 | **PASSED** (PipeWire 1.2.0+) | **REFUSED** (No EIS) | **REFUSED** (No EIS) | **PASSED** (Single-use) | **PASSED** | **PASSED** (PipeWire) | **View-Only** (Typed Refusal) |
| **Traditional X11** | X.Org Server 21.1.13 | Native XShm / XTest | **PASSED** (XShm) | **PASSED** (XTest) | **PASSED** (XTest) | N/A (Session persistent) | **PASSED** (X11 selection) | **PASSED** (PipeWire / Pulse) | **Unconfined Host** (Diagnostics surfaced) |

### Notes on Compositor rows:
1. **GNOME (Mutter):** Supports RemoteDesktop portal and EIS natively through Mutter's built-in EIS implementation. Provides monotonic `pipewire.serial` and supports single-use restore token rotation.
2. **KDE Plasma (KWin):** KWin 6.1+ includes full RemoteDesktop portal and EIS support. Restore tokens are rotated per session establishment.
3. **Hyprland / wlroots:** ScreenCast capture via PipeWire is fully qualified. However, `xdg-desktop-portal-hyprland` does not implement the `RemoteDesktop` interface or EIS virtual device injection. In accordance with Plan §10.1, this row is qualified as **View-Only** with an actionable typed refusal; no privileged or root uinput fallback is attempted.
4. **X11:** Explicit adapter supported for legacy environments. Security diagnostics explicitly warn about the absence of per-window isolation and global input snooping risks.
