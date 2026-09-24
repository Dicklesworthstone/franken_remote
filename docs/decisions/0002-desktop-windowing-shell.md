# ADR 0002: Desktop Client Windowing, Input Capture, and Surface Presentation

- **Status**: Accepted
- **Date**: 2026-09-19
- **Author**: StormyRidge / FrankenRemote Team
- **Tracking Issue**: `fr-p0-decision-notes-85c`
- **Plan References**: [Plan Section 8.3](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#83-target-adapters), [§15.1](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#151-platform-capability-matrix), [§16.1](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#161-shared-core-thin-native-shells), [§25](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#25-risks-bounded-open-decisions-and-rejected-scope)
- **Constitutional Reference**: [AGENTS.md Section 3.1](../../AGENTS.md#31-one-runtime), [§3.5](../../AGENTS.md#35-closed-dependency-universe), [§3.7](../../AGENTS.md#37-size-discipline)

> **Implementation status (corrected 2026-09-24).** This ADR records the intended
> architecture. What exists today: on Linux, `fr` decodes HEVC in software
> (libavcodec) and presents with `XPutImage` from CPU memory
> (`crates/fr-native/src/bridge.c`). There is no MIT-SHM/XPresent path, no
> hardware-decoded surface and no zero-copy presentation. The macOS (AppKit/Metal)
> and Windows (Win32/D3D) shells are not implemented.

---

## 1. Context and Problem Statement

FrankenRemote's desktop client (`fr`) must display high-framerate, hardware-decoded HEVC video on local screens, capture user mouse and keyboard input with sub-millisecond dispatch, handle window events (focus loss/gain, display reconfiguration, DPI scaling), provide system shortcuts, and display minimal session controls (machine name, connection quality, toolbar).

Plan §16.1 mandates:
> "Share connection setup, protocol parsing, session state, stream recovery, input semantics, quality feedback, and diagnostics in Rust. Do not force every platform into an identical event-loop or rendering abstraction when the native surface API is the important performance boundary. Use small native shells for window creation, menus, permissions, and display presentation. AppKit/Metal, Win32/D3D, and an appropriate Linux windowing layer are candidates. Choose one minimal Rust windowing integration where it preserves hardware-surface interoperability... No Electron or bundled Chromium in the daemon or desktop client."

We must select the desktop windowing and shell architecture, bounding dependency bloat and guaranteeing zero-copy hardware swapchain presentation.

---

## 2. Decision

FrankenRemote adopts **thin, platform-native windowing shells** integrated directly with native hardware presentation surfaces per OS, paired with a **shared Rust core (`fr-client`)**:

1. **Per-Platform Hardware-Surface Presentation**:
   - **macOS**: Native AppKit window hosting a hardware-backed `CAMetalLayer`, directly receiving hardware-decoded frames from VideoToolbox.
   - **Windows**: Win32 window hosting a Direct3D 11/12 DXGI swapchain, directly presenting D3D11 textures from NVENC/AMF/QSV hardware decoders.
   - **Linux**: X11/XCB window with MIT-SHM/XPresent (and Wayland subsurfaces via `libdecor`/Wayland portal) presenting hardware-decoded surfaces directly.
2. **Shared Core Ownership (`fr-client`)**:
   - Connection setup, protocol framing, monotonic lease renewal, input sequence numbering with expiry tickets, adaptive quality control, and latency metrics remain 100% in safe Rust.
   - Windowing adapters only emit window events (`FocusLost`, `FocusGained`, `Resize`, `CloseRequested`) and pass raw input events (`KeyDown`, `KeyUp`, `PointerMove`, `PointerButton`) into `fr-client`.
3. **No Heavyweight UI Framework**:
   - Windowing and input capture are handled via minimal, project-audited native shims (`crates/fr-native/src/viewer_window.c`, `crates/fr-native/src/viewer_input.c`) or narrow platform bindings, avoiding multi-megabyte GUI framework runtimes.
   - Shell toolbar and shortcut logic use pure state models (`crates/fr-client/src/toolbar.rs`, `crates/fr-client/src/shortcut.rs`) requiring zero GUI dependencies in the core.
4. **Strict Safety and Idle Discipline**:
   - Focus loss terminates or suspends remote input leases immediately (`on_focus_loss`), preventing background key leakage or stale input execution.
   - Static screens consume zero UI redraw cycles; presentation updates only when new decoded frames arrive or UI chrome state changes.

---

## 3. Evidence Rows

This decision rests on the following implementation and benchmark evidence:

1. **X11 Viewer Window Implementation and Tests** (`crates/fr-native/src/viewer_window.rs`, `tests/viewer_window_x11.rs`):
   - Direct XCB/X11 window creation and event processing cleanly separated from the media worker and async broker.
   - Tested map/unmap, expose handling, and window destruction without blocking or deadlocks.
   - Zero background threads or polling loops: uses single registered waker on event fd.
2. **Desktop Shell Shortcut Matrix and Focus Isolation** (`crates/fr-client/src/shortcut.rs`, `tests/desktop_shell.rs`):
   - Implemented and passed all 10 desktop shell contract tests.
   - Validated platform shortcut differences (macOS Command/Option vs Windows/Linux Ctrl/Alt/Super), emergency break sequence (`Ctrl+Alt+Shift+Escape`), and immediate input lease release on window focus loss.
3. **Toolbar Model Resource Footprint** (`crates/fr-client/src/toolbar.rs`):
   - Pure synchronous state model for connection status, FQDN display, shortcut toggles, and round-trip latency.
   - Zero GUI allocations, zero unsafe code (`#![forbid(unsafe_code)]`), full test coverage in safe Rust.
4. **Presentation Fit and Aspect Ratio Preservation** (`crates/fr-native/src/presentation_fit.rs`):
   - Integer and fixed-point letterbox/pillarbox calculation for arbitrary display aspect ratios without CPU readbacks or GPU blit copies.

---

## 4. Rejected Alternatives

| Alternative | Rejection Reason |
|-------------|------------------|
| **Electron / Bundled Chromium** | Disallowed by Plan §16.1 and AGENTS.md §3.5; adds 150+ MB disk overhead, 100+ MB idle RAM, garbage collection pauses that violate the 50 ms latency target, and prevents direct OS swapchain presentation. |
| **Immediate-Mode Rust GUI (`egui`)** | Requires continuous 60 Hz repaint loops that defeat idle power savings and CPU quietness; forces GPU texture uploads for every video frame, degrading memory bandwidth and latency. |
| **Retained-Mode Rust GUI (`Iced`, `Slint`)** | Incomplete hardware video overlay support on Linux and Windows; brings in secondary async runtimes or threading models incompatible with Asupersync; high code size impact. |
| **Pure `winit` as Complete Shell** | `winit` does not provide native menu bars, system tray icons, accessibility APIs (AT-SPI, UI Automation, NSAccessibility), or platform-specific shortcut grab hooks required by Plan §15.1. |
| **Tauri with Web View** | Adds cross-process IPC hops between the web view and native media pipeline, introducing frame jitter and memory copies on video presentation. |

---

## 5. Revisit Conditions

This decision may be revisited only if:

1. **Lightweight Cross-Platform Native Swapchain Crate**: A modern, lightweight (<5,000 LOC), Rust-native windowing crate emerges that provides zero-cost embedding of raw OS swapchains (`CAMetalLayer`, `HWND` DXGI swapchain, Wayland subsurface), has zero non-Asupersync runtime dependencies, and implements native platform menu bars and accessibility.
2. **Wayland Portal Evolution**: Wayland compositors standardize a universal, zero-copy, direct-scanout presentation interface across GNOME, KDE, and wlroots that replaces X11/XCB mechanisms with a unified safe protocol.
