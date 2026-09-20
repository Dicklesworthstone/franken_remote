#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Native drawable evidence only: independent X11 clear and pixel readback.
//! No synthetic visibility report, decoder completion, or hardware claim.
#[path = "presentation_support/mod.rs"]
mod support;
use fr_core::limits::ProtocolLimits;
use fr_native::{FittedFrame, NativeError, X11Surface};
use support::{Server, picture};

#[test]
fn exposures_before_first_picture_do_not_invent_a_frame_or_allocate_pixels() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    server.clear(target.window(), 2);
    for _ in 0..10 {
        let idle = window.maintain_presentation().unwrap();
        assert!(!idle.repainted);
        assert_eq!(idle.retained_bytes, 0);
    }
}

#[test]
fn idle_exposure_restores_last_picture_without_another_present() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
    let frame = picture(77);
    renderer.present(&frame).unwrap();
    for _ in 0..4 {
        server.clear(target.window(), 8);
        assert_ne!(window.snapshot().unwrap().pixels(), frame.pixels());
        let idle = renderer.maintain_presentation().unwrap();
        assert!(idle.repainted);
        assert_eq!(idle.retained_bytes, 64 * 64 * 4);
        assert_eq!(window.snapshot().unwrap().pixels(), frame.pixels());
        for _ in 0..8 {
            let quiet = renderer.maintain_presentation().unwrap();
            assert!(!quiet.repainted);
            assert_eq!(quiet.retained_bytes, 64 * 64 * 4);
        }
    }
    drop(renderer);
    assert!(window.snapshot().is_ok());
}

#[test]
fn newest_submitted_picture_replaces_the_single_retained_image() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    for value in [37, 112, 219] {
        let frame = picture(value);
        window.present(&frame).unwrap();
        server.clear(target.window(), 1);
        let idle = window.maintain_presentation().unwrap();
        assert!(idle.repainted);
        assert_eq!(idle.retained_bytes, frame.pixels().len());
        assert_eq!(window.snapshot().unwrap().pixels(), frame.pixels());
    }
}

#[test]
fn fitted_picture_and_letterbox_bars_survive_an_idle_clear() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let limits = ProtocolLimits::ABSOLUTE;
    let source = fr_native::BgraFrame::new(128, 64, vec![113; 128 * 64 * 4], &limits).unwrap();
    let mut fitted = FittedFrame::new(128, 64, target, limits).unwrap();
    window.present(fitted.render(&source).unwrap()).unwrap();
    let expected = window.snapshot().unwrap();
    server.clear(target.window(), 2);
    assert!(window.maintain_presentation().unwrap().repainted);
    assert_eq!(window.snapshot().unwrap().pixels(), expected.pixels());
    assert_eq!(&expected.pixels()[0..4], &[0, 0, 0, 255]);
    assert_eq!(
        &expected.pixels()[32 * 64 * 4..32 * 64 * 4 + 4],
        &[113, 113, 113, 255]
    );
}

#[test]
fn resize_away_and_back_retires_retained_picture_before_idle_redraw() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    window.present(&picture(77)).unwrap();
    server.clear(target.window(), 2);
    server.resize_roundtrip(target.window());
    assert_eq!(
        window.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(
        window.present(&picture(78)),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(
        window.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
}

#[test]
fn unmap_and_remap_cannot_revive_borrowed_presentation() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
    renderer.present(&picture(77)).unwrap();
    server.unmap_roundtrip(target.window());
    assert_eq!(
        renderer.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(
        renderer.present(&picture(78)),
        Err(NativeError::GeometryChanged)
    );
}

#[test]
fn destroyed_window_is_retired_before_query_or_redraw() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    window.present(&picture(77)).unwrap();
    server.destroy(target.window());
    assert_eq!(
        window.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(
        window.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
}

#[test]
fn event_flood_is_a_bounded_terminal_refusal_not_an_unbounded_drain() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    window.present(&picture(77)).unwrap();
    server.clear(target.window(), 129);
    assert_eq!(
        window.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(
        window.present(&picture(78)),
        Err(NativeError::GeometryChanged)
    );
}

#[test]
fn capture_surface_cannot_be_used_as_a_retained_presentation_owner() {
    let server = Server::start();
    let mut capture = X11Surface::capture(Some(&server.name), ProtocolLimits::ABSOLUTE).unwrap();
    assert_eq!(
        capture.maintain_presentation(),
        Err(NativeError::InvalidConfiguration)
    );
    assert!(capture.snapshot().is_ok());
}
