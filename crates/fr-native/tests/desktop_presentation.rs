//! Integration tests for cross-platform desktop presentation shells (Plan §§14.1, 16.1, ADR 0002).
#![deny(unsafe_op_in_unsafe_fn)]

use fr_native::macos_presentation::{
    AppKitWindowConfig, AppKitWindowEvent, MacOsDesktopShell, MetalPresentationSurface,
};
use fr_native::wayland::{
    WaylandDesktopShell, WaylandDmabufSurface, WaylandWindowConfig, WaylandWindowEvent,
};
use fr_native::windows::{
    DxgiColorSpace, DxgiPresentationSurface, Win32DesktopShell, Win32WindowConfig, Win32WindowEvent,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn windows_desktop_shell_lifecycle_and_presentation() {
    let config = Win32WindowConfig {
        width: 2560,
        height: 1440,
        title: "Workstation - fr".to_string(),
        per_monitor_dpi_v2: true,
    };
    let mut shell = Win32DesktopShell::new(config);
    assert!(!shell.is_focused());
    assert!(!shell.is_closed());

    // Attach DXGI swapchain presentation surface
    let surface = DxgiPresentationSurface::new(2560, 1440, DxgiColorSpace::StudioYcbcrBt709)
        .expect("create surface");
    assert_eq!(surface.width(), 2560);
    assert_eq!(surface.height(), 1440);
    assert_eq!(surface.color_space(), DxgiColorSpace::StudioYcbcrBt709);
    assert_eq!(surface.buffer_count(), 2);
    assert!(surface.allow_tearing());
    shell.attach_surface(surface);

    // Present decoded frames directly to swapchain without readback
    let surface_mut = shell.surface_mut().expect("surface exists");
    surface_mut
        .present_hardware_surface(0x1234_5678)
        .expect("present texture");
    assert_eq!(surface_mut.presented_frames(), 1);

    // Focus gained
    let focus_lost_triggered = AtomicBool::new(false);
    shell
        .process_event(Win32WindowEvent::FocusGained, || {
            focus_lost_triggered.store(true, Ordering::SeqCst);
        })
        .expect("process focus gained");
    assert!(shell.is_focused());
    assert!(!focus_lost_triggered.load(Ordering::SeqCst));

    // Focus lost (WM_KILLFOCUS) must trigger immediate on_focus_loss callback!
    shell
        .process_event(Win32WindowEvent::FocusLost, || {
            focus_lost_triggered.store(true, Ordering::SeqCst);
        })
        .expect("process focus lost");
    assert!(!shell.is_focused());
    assert!(focus_lost_triggered.load(Ordering::SeqCst));

    // Window resize updates swapchain buffers
    shell
        .process_event(
            Win32WindowEvent::Resized {
                width: 1920,
                height: 1080,
            },
            || {},
        )
        .expect("resize buffers");
    let s = shell.surface().unwrap();
    assert_eq!(s.width(), 1920);
    assert_eq!(s.height(), 1080);

    // Close requested sets closed state and invokes on_focus_loss
    let close_cleanup_triggered = AtomicBool::new(false);
    shell
        .process_event(Win32WindowEvent::CloseRequested, || {
            close_cleanup_triggered.store(true, Ordering::SeqCst);
        })
        .expect("process close");
    assert!(shell.is_closed());
    assert!(close_cleanup_triggered.load(Ordering::SeqCst));
}

#[test]
fn macos_desktop_shell_lifecycle_and_presentation() {
    let config = AppKitWindowConfig {
        width: 2880,
        height: 1800,
        title: "MacBook Pro - fr".to_string(),
        backing_scale_factor: 2.0,
    };
    let mut shell = MacOsDesktopShell::new(config);
    assert!(!shell.is_focused());
    assert!(!shell.is_closed());

    // Attach CAMetalLayer presentation surface
    let surface =
        MetalPresentationSurface::new(2880, 1800, 2.0, 1001).expect("create metal surface");
    assert_eq!(surface.width(), 2880);
    assert_eq!(surface.height(), 1800);
    assert_eq!(surface.backing_scale(), 2.0);
    assert_eq!(surface.metal_device_id(), 1001);
    shell.attach_surface(surface);

    // Present decoded CVPixelBuffer directly without CPU readback
    let surface_mut = shell.surface_mut().expect("surface exists");
    surface_mut
        .present_hardware_surface(0xfeed_beef)
        .expect("present pixel buffer");
    assert_eq!(surface_mut.presented_frames(), 1);

    // Window focus gained
    let focus_lost_triggered = AtomicBool::new(false);
    shell
        .process_event(AppKitWindowEvent::FocusGained, || {
            focus_lost_triggered.store(true, Ordering::SeqCst);
        })
        .expect("focus gained");
    assert!(shell.is_focused());

    // Window focus lost (windowDidResignKey) immediately invokes on_focus_loss
    shell
        .process_event(AppKitWindowEvent::FocusLost, || {
            focus_lost_triggered.store(true, Ordering::SeqCst);
        })
        .expect("focus lost");
    assert!(!shell.is_focused());
    assert!(focus_lost_triggered.load(Ordering::SeqCst));

    // Resize drawable
    shell
        .process_event(
            AppKitWindowEvent::Resized {
                width: 1440,
                height: 900,
            },
            || {},
        )
        .expect("resize");
    assert_eq!(shell.surface().unwrap().width(), 1440);
    assert_eq!(shell.surface().unwrap().height(), 900);
}

#[test]
fn wayland_desktop_shell_lifecycle_and_presentation() {
    let config = WaylandWindowConfig {
        width: 1920,
        height: 1080,
        title: "Wayland Session - fr".to_string(),
        app_id: "com.frankenremote.client".to_string(),
    };
    let mut shell = WaylandDesktopShell::new(config);
    assert!(!shell.is_focused());
    assert!(!shell.is_closed());

    // Attach dmabuf surface
    let surface =
        WaylandDmabufSurface::new(1920, 1080, 0x3231_564e).expect("create dmabuf surface"); // DRM_FORMAT_NV12
    assert_eq!(surface.width(), 1920);
    assert_eq!(surface.height(), 1080);
    assert_eq!(surface.fourcc_format(), 0x3231_564e);
    shell.attach_surface(surface);

    // Present hardware-decoded dmabuf directly without readback
    let surface_mut = shell.surface_mut().expect("surface exists");
    surface_mut
        .present_dmabuf_buffer(5)
        .expect("present dmabuf");
    assert_eq!(surface_mut.presented_frames(), 1);

    // Focus loss triggers on_focus_loss immediately
    let focus_lost_triggered = AtomicBool::new(false);
    shell
        .process_event(WaylandWindowEvent::FocusGained, || {})
        .expect("focus gained");
    assert!(shell.is_focused());

    shell
        .process_event(WaylandWindowEvent::FocusLost, || {
            focus_lost_triggered.store(true, Ordering::SeqCst);
        })
        .expect("focus lost");
    assert!(!shell.is_focused());
    assert!(focus_lost_triggered.load(Ordering::SeqCst));
}
