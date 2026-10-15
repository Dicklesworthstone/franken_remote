#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Exercise the actual cursor repaint helper called by fr-media-worker against
//! independent X11 readback and native transfer counters. No codec/IPC claim.
#[allow(dead_code)]
#[path = "presentation_support/mod.rs"]
mod support;
#[path = "../src/bin/media_worker/cursor.rs"]
mod worker_cursor;
use fr_core::limits::ProtocolLimits;
use fr_media::worker::{overlay::Update, presentation::Fit};
use fr_native::{
    BgraFrame, CursorOverlay, NativeError, OverlayPlacement, X11Surface, image_transfer::Path,
};
use support::{Server, picture};

fn fresh(value: u8) -> BgraFrame {
    picture(value)
}

fn assert_pixels(surface: &mut X11Surface, expected: &BgraFrame) {
    assert_eq!(surface.snapshot().unwrap().pixels(), expected.pixels());
}

#[test]
fn worker_cursor_moves_repaint_only_their_rectangles_on_shared_memory() {
    updates(&Server::start(), true);
}
#[test]
fn worker_cursor_moves_repaint_only_their_rectangles_on_socket_fallback() {
    updates(&Server::without_shared_memory(), false);
}

#[allow(clippy::too_many_lines)] // Keep one native-state/counter scenario together.
fn updates(server: &Server, shared: bool) {
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
    let rgba = [251, 7, 19, 128].repeat(4 * 4);
    let mut overlay = CursorOverlay::new();
    let mut f = fresh(31);
    overlay
        .apply(&Update::Shape {
            x: 4,
            y: 8,
            width: 4,
            height: 4,
            hotspot_x: 1,
            hotspot_y: 1,
            rgba: &rgba,
        })
        .unwrap();
    overlay
        .composite_fresh(&mut f, OverlayPlacement::Identity)
        .unwrap();
    renderer.present(&f).unwrap();
    renderer.maintain_presentation().unwrap();
    assert_pixels(&mut window, &f);
    assert_eq!(
        matches!(
            renderer.transfer_statistics().unwrap().path,
            Path::SharedMemory
        ),
        shared
    );
    let retained = overlay.retained_bytes();
    // Each byte count is independently specified, not calculated from the
    // implementation's damage report. A full-frame repaint fails immediately.
    for (update, bytes, images) in [
        (Update::Move { x: 50, y: 55 }, 128, 2),
        (Update::Move { x: 50, y: 55 }, 0, 0),
        (Update::Move { x: 63, y: 63 }, 80, 2),
        (Update::Hidden, 16, 1),
        (Update::Hidden, 0, 0),
        (Update::Move { x: -100, y: -100 }, 0, 0),
        (Update::Move { x: 5, y: 9 }, 64, 1),
    ] {
        overlay.apply(&update).unwrap();
        let before = renderer.transfer_statistics().unwrap();
        worker_cursor::repaint(
            &mut renderer,
            &mut overlay,
            &mut f,
            OverlayPlacement::Identity,
        )
        .unwrap();
        assert_pixels(&mut window, &f);
        let after = renderer.transfer_statistics().unwrap();
        assert_eq!(after.copied_bytes - before.copied_bytes, bytes);
        assert_eq!(
            after.socket_pixel_bytes - before.socket_pixel_bytes,
            if shared { 0 } else { bytes }
        );
        assert_eq!(after.images - before.images, images);
        assert_eq!(after.retained_bytes, before.retained_bytes);
        assert_eq!(overlay.retained_bytes(), retained);
        // Read the retained C image back after independent exposure too: a
        // hidden full-frame copy would otherwise escape visible-only checks.
        server.clear(target.window(), 1);
        renderer.maintain_presentation().unwrap();
        assert_pixels(&mut window, &f);
        assert_eq!(
            renderer.transfer_statistics().unwrap().copied_bytes,
            after.copied_bytes
        );
    }
    // Replace the shape at the same bounds; it must repaint once, not no-op.
    let next = [1, 233, 41, 192].repeat(16);
    overlay
        .apply(&Update::Shape {
            x: 5,
            y: 9,
            width: 4,
            height: 4,
            hotspot_x: 1,
            hotspot_y: 1,
            rgba: &next,
        })
        .unwrap();
    let before = renderer.transfer_statistics().unwrap();
    worker_cursor::repaint(
        &mut renderer,
        &mut overlay,
        &mut f,
        OverlayPlacement::Identity,
    )
    .unwrap();
    assert_pixels(&mut window, &f);
    assert_eq!(
        renderer.transfer_statistics().unwrap().copied_bytes - before.copied_bytes,
        64
    );
    overlay.apply(&Update::Hidden).unwrap();
    worker_cursor::repaint(
        &mut renderer,
        &mut overlay,
        &mut f,
        OverlayPlacement::Identity,
    )
    .unwrap();
    assert_pixels(&mut window, &fresh(31));
}

#[test]
fn an_empty_cursor_update_repairs_exposure_without_a_copy_and_still_fences_the_window() {
    for server in [Server::start(), Server::without_shared_memory()] {
        let mut window = server.window();
        let target = window.presentation_target().unwrap();
        let mut renderer =
            X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
        let mut f = fresh(81);
        let mut overlay = CursorOverlay::new();
        overlay
            .composite_fresh(&mut f, OverlayPlacement::Identity)
            .unwrap();
        renderer.present(&f).unwrap();
        renderer.maintain_presentation().unwrap();
        let before = renderer.transfer_statistics().unwrap();
        server.clear(target.window(), 1);
        worker_cursor::repaint(
            &mut renderer,
            &mut overlay,
            &mut f,
            OverlayPlacement::Identity,
        )
        .unwrap();
        assert_pixels(&mut window, &f);
        assert_eq!(
            renderer.transfer_statistics().unwrap().copied_bytes,
            before.copied_bytes
        );
        server.resize_roundtrip(target.window());
        assert_eq!(
            worker_cursor::repaint(
                &mut renderer,
                &mut overlay,
                &mut f,
                OverlayPlacement::Identity
            ),
            Err(NativeError::GeometryChanged)
        );
    }
}

#[test]
fn fitted_cursor_damage_does_not_transfer_letterbox_or_intervening_desktop_pixels() {
    for server in [Server::start(), Server::without_shared_memory()] {
        let mut window = server.window();
        let target = window.presentation_target().unwrap();
        let mut renderer =
            X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
        let placement = OverlayPlacement::Fitted {
            fit: Fit {
                x: 0,
                y: 16,
                width: 64,
                height: 32,
            },
            source_width: 128,
            source_height: 64,
        };
        let mut overlay = CursorOverlay::new();
        let rgba = [231, 13, 77, 255].repeat(16);
        let mut f = fresh(99);
        overlay
            .apply(&Update::Shape {
                x: 40,
                y: 10,
                width: 4,
                height: 4,
                hotspot_x: 0,
                hotspot_y: 0,
                rgba: &rgba,
            })
            .unwrap();
        overlay.composite_fresh(&mut f, placement).unwrap();
        renderer.present(&f).unwrap();
        renderer.maintain_presentation().unwrap();
        let before = renderer.transfer_statistics().unwrap();
        overlay.apply(&Update::Move { x: 0, y: -6 }).unwrap();
        worker_cursor::repaint(&mut renderer, &mut overlay, &mut f, placement).unwrap();
        assert_pixels(&mut window, &f);
        // Old 4x4 plus one clipped 4x1 row at the picture's top edge.
        assert_eq!(
            renderer.transfer_statistics().unwrap().copied_bytes - before.copied_bytes,
            80
        );
        let expected = fresh(99);
        for y in (0..16).chain(48..64) {
            let at = y * 64 * 4;
            assert_eq!(
                &f.pixels()[at..at + 64 * 4],
                &expected.pixels()[at..at + 64 * 4]
            );
        }
    }
}

#[test]
fn a_fresh_video_picture_replaces_the_saved_background_before_cursor_only_updates_resume() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
    let mut overlay = CursorOverlay::new();
    let rgba = [101, 13, 239, 128].repeat(16);
    overlay
        .apply(&Update::Shape {
            x: 20,
            y: 20,
            width: 4,
            height: 4,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: &rgba,
        })
        .unwrap();
    for value in [27, 113, 191] {
        let mut f = fresh(value);
        overlay.apply(&Update::Move { x: 20, y: 20 }).unwrap();
        overlay
            .composite_fresh(&mut f, OverlayPlacement::Identity)
            .unwrap();
        renderer.present(&f).unwrap();
        renderer.maintain_presentation().unwrap();
        assert_pixels(&mut window, &f);
        let before = renderer.transfer_statistics().unwrap();
        overlay.apply(&Update::Hidden).unwrap();
        worker_cursor::repaint(
            &mut renderer,
            &mut overlay,
            &mut f,
            OverlayPlacement::Identity,
        )
        .unwrap();
        assert_eq!(
            renderer.transfer_statistics().unwrap().copied_bytes - before.copied_bytes,
            64
        );
        assert_pixels(&mut window, &fresh(value));
    }
}
