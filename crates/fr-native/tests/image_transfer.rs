#![cfg(all(target_os = "linux", feature = "linux-displays"))]
//! Real X11 root and selected-source integration, not transport/GPU evidence.
#[path = "display_inventory/support.rs"]
mod support;
use fr_core::limits::ProtocolLimits;
use fr_native::{NativeError, X11Surface, image_transfer::Path};
use support::Screen;

#[test]
fn root_capture_reuses_one_shared_buffer_and_counts_actual_copies() {
    let screen = Screen::start();
    let mut surface = X11Surface::capture(Some(&screen.name), ProtocolLimits::ABSOLUTE).unwrap();
    assert_eq!(
        surface.transfer_statistics().unwrap().path,
        Path::NotInitialized
    );
    for color in [0xff_0000, 0x00_ff00, 0x00_00ff] {
        screen.paint(0, 640, color);
        let frame = surface.snapshot().unwrap();
        let expected = [
            u8::try_from(color & 255).unwrap(),
            u8::try_from((color >> 8) & 255).unwrap(),
            u8::try_from((color >> 16) & 255).unwrap(),
            255,
        ];
        assert!(
            frame
                .pixels()
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == expected)
        );
    }
    let stats = surface.transfer_statistics().unwrap();
    assert_eq!(stats.path, Path::SharedMemory);
    assert_eq!(stats.images, 3);
    assert_eq!(stats.copied_bytes, 3 * 640 * 480 * 4);
    assert_eq!(stats.socket_pixel_bytes, 0);
    assert_eq!(stats.retained_bytes, 640 * 480 * 4);
}

#[test]
fn selected_shared_readback_never_copies_neighboring_monitor_pixels() {
    let screen = Screen::start();
    screen.monitor("left", 0, 0, 320, 480);
    screen.monitor("right", 320, 0, 320, 480);
    screen.paint(0, 320, 0xff_0000);
    screen.paint(320, 320, 0x00_00ff);
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    let display = catalog
        .displays()
        .iter()
        .find(|display| display.x == 320)
        .unwrap();
    let mut capture = inventory.select(display.handle).unwrap();
    let frame = capture.snapshot().unwrap();
    assert!(
        frame
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [255, 0, 0, 255])
    );
    screen.paint(0, 320, 0x00_ff00);
    assert_eq!(capture.snapshot().unwrap().pixels(), frame.pixels());
    let stats = capture.transfer_statistics().unwrap();
    assert_eq!(stats.path, Path::SharedMemory);
    assert_eq!(stats.images, 2);
    assert_eq!(stats.copied_bytes, 2 * 320 * 480 * 4);
    assert_eq!(stats.retained_bytes, 320 * 480 * 4);
    assert_eq!(stats.socket_pixel_bytes, 0);
    screen.remove("right");
    screen.monitor("right", 320, 0, 320, 480);
    assert_eq!(capture.snapshot().err(), Some(NativeError::GeometryChanged));
    assert_eq!(capture.snapshot().err(), Some(NativeError::Closed));
    assert_eq!(capture.transfer_statistics().unwrap().images, 2);
}

#[test]
fn simultaneous_original_connections_do_not_share_image_or_error_state() {
    let screen = Screen::start();
    screen.paint(0, 640, 0x24_68ac);
    std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let name = &screen.name;
                scope.spawn(move || {
                    let mut capture =
                        X11Surface::capture(Some(name), ProtocolLimits::ABSOLUTE).unwrap();
                    for _ in 0..8 {
                        assert!(
                            capture
                                .snapshot()
                                .unwrap()
                                .pixels()
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .all(|pixel| *pixel == [0xac, 0x68, 0x24, 255])
                        );
                    }
                    assert_eq!(capture.transfer_statistics().unwrap().images, 8);
                    assert_eq!(
                        capture.transfer_statistics().unwrap().path,
                        Path::SharedMemory
                    );
                })
            })
            .collect();
        for task in tasks {
            task.join().unwrap();
        }
    });
}
