#![cfg(all(target_os = "linux", feature = "linux-media"))]
mod cursor_support;
use cursor_support::Server;
use fr_core::{
    ids::CodecConfigurationGeneration,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_media::{
    access_unit::FrameId,
    worker::{Backend, Configuration},
};
use fr_native::{
    EncodeBackend, HevcEncoder, NativeError, X11Surface,
    capture::{CaptureOutput, ChangeAwareCapture},
};

fn sample(surface: &mut X11Surface) -> fr_native::cursor::CursorSnapshot {
    surface.capture_cursor().unwrap().unwrap()
}
#[test]
fn original_root_observes_real_shape_hotspot_and_position_without_pixels() {
    let server = Server::start(true);
    let mut capture = X11Surface::capture(Some(&server.name), ProtocolLimits::ABSOLUTE).unwrap();
    server.cursor(8, 4, (2, 1), 0xffff_0000);
    server.warp(37, 59);
    let first = sample(&mut capture);
    assert_eq!(first.position(), (37, 59));
    let shape = first.shape(7);
    assert_eq!(
        (shape.width, shape.height, shape.hotspot_x, shape.hotspot_y),
        (8, 4, 2, 1)
    );
    assert_eq!(shape.rgba, [255, 0, 0, 255].repeat(32));
    assert_eq!(shape.flags, 0, "native image cannot certify visibility");
    server.warp(123, 71);
    let second = sample(&mut capture);
    assert_eq!(second.native_serial(), first.native_serial());
    assert_eq!(second.position(), (123, 71));
    assert_eq!(second.shape(7), first.shape(7));
    server.cursor(4, 8, (1, 2), 0xff00_ff00);
    let third = sample(&mut capture);
    assert_ne!(third.native_serial(), first.native_serial());
    assert_eq!(third.shape(8).rgba, [0, 255, 0, 255].repeat(32));
    assert!(!format!("{third:?}").contains("rgba"));
}
#[test]
fn premultiplied_native_alpha_is_converted_without_dark_edges() {
    let server = Server::start(true);
    let mut capture = X11Surface::capture(Some(&server.name), ProtocolLimits::ABSOLUTE).unwrap();
    server.cursor(4, 4, (0, 0), 0x8040_2010);
    let observed = sample(&mut capture);
    assert_eq!(observed.shape(1).rgba, [128, 64, 32, 128].repeat(16));
    server.cursor(4, 4, (0, 0), 0);
    assert_eq!(sample(&mut capture).shape(2).rgba, [0; 64]);
}
#[test]
fn server_hiding_is_not_misreported_as_known_visible_cursor() {
    let server = Server::start(true);
    let mut capture = X11Surface::capture(Some(&server.name), ProtocolLimits::ABSOLUTE).unwrap();
    server.cursor(4, 4, (0, 0), 0xff00_ff00);
    let before = sample(&mut capture);
    server.hide();
    let hidden = sample(&mut capture);
    // The real server still returns the logical image: preserve this negative
    // evidence and NEVER derive a visibility flag from successful shape capture.
    assert_eq!(hidden.shape(1).rgba, before.shape(1).rgba);
    assert!(!hidden.shape(1).is_visible());
}
#[test]
fn absent_xfixes_and_presenter_roles_refuse_without_fabricated_cursor() {
    let server = Server::start(false);
    let mut capture = X11Surface::capture(Some(&server.name), ProtocolLimits::ABSOLUTE).unwrap();
    assert!(matches!(
        capture.capture_cursor(),
        Err(NativeError::Unavailable)
    ));
    assert!(capture.snapshot().is_ok());
    let server = Server::start(true);
    let mut presenter =
        X11Surface::presenter(Some(&server.name), 64, 64, ProtocolLimits::ABSOLUTE).unwrap();
    assert!(matches!(
        presenter.capture_cursor(),
        Err(NativeError::Unavailable)
    ));
}
#[test]
fn negotiated_shape_bound_applies_before_rust_pixel_copy() {
    let server = Server::start(true);
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(1024),
        ..LimitOverrides::default()
    })
    .unwrap();
    let mut capture = X11Surface::capture(Some(&server.name), limits).unwrap();
    server.cursor(16, 16, (0, 0), 0xff00_ff00);
    assert!(matches!(
        capture.capture_cursor(),
        Err(NativeError::InvalidConfiguration)
    ));
    server.cursor(4, 4, (0, 0), 0xff00_ff00);
    assert_eq!(sample(&mut capture).shape(1).rgba.len(), 64);
}
#[test]
fn cursor_changes_do_not_trigger_a_frame_or_modify_capture_freshness() {
    let server = Server::start(true);
    let config = Configuration {
        width: 640,
        height: 480,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    };
    let surface = X11Surface::capture(Some(&server.name), config.limits().unwrap()).unwrap();
    let encoder = HevcEncoder::new(
        config.codec().unwrap(),
        config.limits().unwrap(),
        EncodeBackend::SoftwareExplicit,
        30,
        config.bitrate,
    )
    .unwrap();
    let mut capture = ChangeAwareCapture::new(surface, encoder);
    server.cursor(8, 8, (0, 0), 0xff00_ff00);
    assert_eq!(
        capture.capture(FrameId::FIRST, 1, true, true).unwrap(),
        CaptureOutput::Submitted
    );
    capture.poll_output().unwrap();
    let stats = capture.stats();
    for x in 0..20 {
        server.warp(x, 10);
        assert!(capture.capture_cursor().unwrap().is_some());
    }
    assert_eq!(
        capture.stats(),
        stats,
        "cursor polling touched HEVC/readback state"
    );
    assert!(matches!(
        capture
            .capture(FrameId::from_raw(1), 2, false, true)
            .unwrap(),
        CaptureOutput::Unchanged(_)
    ));
}

#[cfg(feature = "linux-displays")]
#[path = "display_inventory/support.rs"]
mod displays;
#[cfg(feature = "linux-displays")]
#[test]
fn selected_monitor_clips_scope_and_refuses_same_size_replacement() {
    let screen = displays::Screen::start();
    screen.monitor("fr-left", 0, 0, 320, 480);
    screen.monitor("fr-right", 320, 0, 320, 480);
    screen.paint(0, 640, 0);
    let server = Server::connect(&screen.name);
    server.cursor(8, 8, (2, 3), 0xff00_ff00);
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    let id = catalog
        .displays()
        .iter()
        .find(|d| d.x == 320)
        .unwrap()
        .handle;
    let mut capture = inventory.select(id).unwrap();
    server.warp(319, 20);
    assert!(capture.capture_cursor().unwrap().is_none());
    server.warp(320, 20);
    let first = capture.capture_cursor().unwrap().unwrap();
    assert_eq!(first.position(), (0, 20));
    assert_eq!(first.shape(1).hotspot_x, 2);
    assert_eq!(first.shape(1).rgba, [0, 255, 0, 255].repeat(64));
    server.warp(639, 479);
    assert_eq!(
        capture.capture_cursor().unwrap().unwrap().position(),
        (319, 479)
    );
    server.warp(200, 20);
    assert!(capture.capture_cursor().unwrap().is_none());
    // Even a malformed/oversized shape outside the selected area is not copied
    // or emitted. Scope checks precede native bitmap retrieval and validation.
    server.cursor(256, 256, (0, 0), 0xffff_0000);
    assert!(capture.capture_cursor().unwrap().is_none());
    server.warp(350, 20);
    assert!(matches!(
        capture.capture_cursor(),
        Err(NativeError::InvalidConfiguration)
    ));
    server.cursor(8, 8, (0, 0), 0xff00_ff00);
    screen.remove("fr-right");
    screen.monitor("fr-right", 320, 0, 320, 480);
    assert!(matches!(
        capture.capture_cursor(),
        Err(NativeError::GeometryChanged)
    ));
    assert!(matches!(capture.capture_cursor(), Err(NativeError::Closed)));
}
