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

> **Withdrawn 2026-09-24: no evidence existed.** An earlier version of this table
> marked GNOME 46.4/47.0, KDE Plasma 6.1.5/6.2.0 and Hyprland 0.42.0 as PASSED for
> portal capture, EIS input, restore tokens, clipboard and audio, and claimed XShm
> capture on X11. The tree contains no xdg-desktop-portal, PipeWire or libei
> integration (no D-Bus client dependency at all), and X11 capture uses
> `XGetImage` (`crates/fr-native/src/bridge.c`), not XShm. The honest spike record
> [`spikes/os-lifecycle/README.md`](../spikes/os-lifecycle/README.md) lists GNOME
> and KDE as not tested and Hyprland 0.56.2 as blocked. The runtime table
> `frd::linux::QUALIFIED_ROWS` is empty and `evaluate_compositor` refuses every
> compositor with a typed reason.

| Compositor Family | Capture | Pointer | Keyboard | Restore Token | Clipboard | Playback Audio | Status |
|---|---|---|---|---|---|---|---|
| **GNOME (Mutter)** | not tested | not tested | not tested | not tested | not tested | not tested | **blocked**: no portal/PipeWire/libei implementation |
| **KDE Plasma (KWin)** | not tested | not tested | not tested | not tested | not tested | not tested | **blocked**: no portal/PipeWire/libei implementation |
| **Hyprland / wlroots** | blocked (spike: VAAPI driver missing) | not tested | not tested | not tested | not tested | not tested | **blocked**: no portal/PipeWire/libei implementation |
| **X11** | `XGetImage` capture exercised under Xvfb in CI | XTest exercised under Xvfb | XTest exercised under Xvfb | not applicable | X11 selection workers exercised under Xvfb | not tested | **not qualified**: Xvfb test evidence only; no installed-desktop run |

The X11 row reports source/test evidence (Xvfb in CI), a separate category from
hardware or installed-desktop qualification. The design notes in section 1
describe intended Wayland behaviour; none of it is implemented.
