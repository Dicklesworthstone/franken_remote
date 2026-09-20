#![cfg(all(target_os = "linux", feature = "linux-displays"))]
//! Actual `RandR` monitor discovery/changes and selected X11 pixels, not a catalog fixture.
use fr_core::limits::ProtocolLimits;
use fr_native::{NativeError, displays::X11Inventory};
#[path = "display_inventory/support.rs"]
mod support;
use support::Screen;

#[test]
fn actual_randr_catalog_and_unchanged_geometry_are_stable() {
    let screen = Screen::start();
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    assert_eq!(catalog.displays().len(), 1);
    let d = catalog.displays()[0];
    assert_eq!((d.x, d.y, d.pixel_width, d.pixel_height), (0, 0, 640, 480));
    for _ in 0..10 {
        assert_eq!(inventory.catalog().unwrap(), catalog);
    }
    assert_eq!(format!("{catalog:?}"), "DisplayCatalog { count: 1, .. }");
}
#[test]
fn selected_monitor_copies_only_its_pixels_not_the_surrounding_root() {
    let screen = Screen::start();
    screen.monitor("fr-left", 0, 0, 320, 480);
    screen.monitor("fr-right", 320, 0, 320, 480);
    screen.paint(0, 320, 0x00ff_0000);
    screen.paint(320, 320, 0x0000_00ff);
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    let selected = catalog.displays().iter().find(|d| d.x == 320).unwrap();
    let mut capture = inventory.select(selected.handle).unwrap();
    let frame = capture.snapshot().unwrap();
    assert_eq!((frame.width(), frame.height()), (320, 480));
    assert!(
        frame
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [255, 0, 0, 255])
    );
    screen.paint(0, 320, 0x0000_ff00);
    assert_eq!(capture.snapshot().unwrap().pixels(), frame.pixels());
}
#[test]
fn changed_monitor_is_terminal_even_after_bounds_are_restored() {
    let screen = Screen::start();
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    let selected = catalog
        .displays()
        .iter()
        .find(|d| d.pixel_width == 320)
        .unwrap();
    let mut capture = inventory.select(selected.handle).unwrap();
    capture.snapshot().unwrap();
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 320, 0, 320, 480);
    assert_eq!(capture.snapshot().err(), Some(NativeError::GeometryChanged));
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    assert_eq!(capture.snapshot().err(), Some(NativeError::Closed));
}
#[test]
fn remove_readd_with_identical_identity_and_size_does_not_revive_inventory() {
    let screen = Screen::start();
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    let mut inventory = screen.inventory();
    let original = inventory.catalog().unwrap();
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    // Final metadata is identical. The queued native resource notification must
    // still fence the old lifetime instead of using equality as proof of identity.
    let fresh = screen.inventory().catalog().unwrap();
    assert_eq!(fresh, original);
    assert_eq!(
        inventory.catalog().err(),
        Some(NativeError::GeometryChanged)
    );
    assert_eq!(inventory.catalog().err(), Some(NativeError::Closed));
}
#[test]
fn out_of_scope_handles_and_network_or_malformed_display_selectors_refuse() {
    for name in [
        "",
        ":",
        ":..0",
        ":0.",
        ":0.1.2",
        "127.0.0.1:0",
        "unix:0",
        ":0\0",
    ] {
        assert!(X11Inventory::open(name, ProtocolLimits::ABSOLUTE).is_err());
    }
    let screen = Screen::start();
    assert_eq!(
        screen.inventory().select(u128::MAX).err().unwrap(),
        NativeError::InvalidConfiguration
    );
}
#[test]
fn inventory_capacity_is_enforced_instead_of_truncating_displays() {
    let screen = Screen::start();
    for i in 0..9 {
        screen.monitor(&format!("fr-{i}"), 0, 0, 160, 120);
    }
    assert_eq!(
        X11Inventory::open(&screen.name, ProtocolLimits::ABSOLUTE).err(),
        Some(NativeError::InvalidConfiguration)
    );
}

#[test]
fn topology_changed_after_encode_submission_cannot_release_the_old_picture() {
    use fr_core::ids::CodecConfigurationGeneration;
    use fr_media::{
        access_unit::FrameId,
        worker::{Backend, Configuration},
    };
    use fr_native::{
        EncodeBackend, HevcEncoder,
        capture::{CaptureOutput, ChangeAwareCapture},
    };
    let screen = Screen::start();
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    let display = catalog
        .displays()
        .iter()
        .find(|d| d.pixel_width == 320)
        .unwrap();
    let surface = inventory.select(display.handle).unwrap();
    let config = Configuration {
        width: 320,
        height: 480,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 4_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    };
    let encoder = HevcEncoder::new(
        config.codec().unwrap(),
        config.limits().unwrap(),
        EncodeBackend::SoftwareExplicit,
        u32::from(config.fps),
        config.bitrate,
    )
    .unwrap();
    let mut capture = ChangeAwareCapture::selected(surface, encoder);
    assert_eq!(
        capture.capture(FrameId::FIRST, 100, true, false).unwrap(),
        CaptureOutput::Submitted
    );
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    assert_eq!(
        capture.poll_output().err(),
        Some(NativeError::GeometryChanged)
    );
    assert_eq!(capture.poll_output().err(), Some(NativeError::Closed));
    assert_eq!(
        capture
            .capture(FrameId::from_raw(2), 200, false, true)
            .err(),
        Some(NativeError::Closed)
    );
}

#[path = "display_inventory/worker.rs"]
mod selected_worker;

#[test]
fn full_screen_capture_also_fences_topology_changes_after_encode_submission() {
    use fr_core::ids::CodecConfigurationGeneration;
    use fr_media::{
        access_unit::FrameId,
        worker::{Backend, Configuration},
    };
    use fr_native::{
        EncodeBackend, HevcEncoder,
        capture::{CaptureOutput, ChangeAwareCapture},
    };
    let screen = Screen::start();
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    let surface =
        fr_native::X11Surface::capture(Some(&screen.name), ProtocolLimits::ABSOLUTE).unwrap();
    let config = Configuration {
        width: 640,
        height: 480,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 4_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    };
    let encoder = HevcEncoder::new(
        config.codec().unwrap(),
        config.limits().unwrap(),
        EncodeBackend::SoftwareExplicit,
        u32::from(config.fps),
        config.bitrate,
    )
    .unwrap();
    let mut capture = ChangeAwareCapture::new(surface, encoder);
    assert_eq!(
        capture.capture(FrameId::FIRST, 100, true, false).unwrap(),
        CaptureOutput::Submitted
    );
    screen.remove("fr-monitor");
    screen.monitor("fr-monitor", 0, 0, 320, 480);
    assert_eq!(
        capture.poll_output().err(),
        Some(NativeError::GeometryChanged)
    );
    assert_eq!(capture.poll_output().err(), Some(NativeError::Closed));
    assert_eq!(
        capture
            .capture(FrameId::from_raw(2), 200, false, true)
            .err(),
        Some(NativeError::Closed)
    );
}
