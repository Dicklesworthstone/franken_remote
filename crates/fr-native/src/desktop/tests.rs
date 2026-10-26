//! Coordinate policy only; fixture grants are not native input authorization.
use super::*;
use fr_client::input::{ClientInstant, InputClient, Policy, viewport::SurfaceRect};
use fr_core::{ids::*, input::*, input_submission::Capabilities, limits::ProtocolLimits};

#[test]
fn file_panel_directions_retain_distinct_status_and_both_cleanup_owners() {
    use crate::file_progress::{Panel, Status as PanelStatus, StopReason};
    use std::time::{Duration, Instant};
    for upload_finishes_first in [true, false] {
        let configuration =
            Configuration::new(Path::new("/usr/bin/false"), ":0", None, 79).unwrap();
        let mut desktop = Desktop::new(configuration);
        let (upload, finish_upload) = Panel::pending_cleanup_fixture();
        let (receive, finish_receive) = Panel::pending_cleanup_fixture();
        desktop.file_progress = Some(Ok(upload));
        desktop.receive_progress = Some(Ok(receive));
        let upload = desktop.upload_progress().unwrap().unwrap();
        let receive = desktop.receive_progress().unwrap().unwrap();
        upload.hide();
        assert_eq!(upload.status(), PanelStatus::Stopped(StopReason::Hidden));
        assert_eq!(receive.status(), PanelStatus::Running);
        assert_eq!(
            desktop.file_progress().unwrap().unwrap().status(),
            upload.status()
        );
        desktop.close();
        assert_eq!(
            receive.status(),
            PanelStatus::Stopped(StopReason::OwnerStopped)
        );
        assert_eq!(desktop.window_cleanup(), WindowCleanup::Pending);
        let (first, last) = if upload_finishes_first {
            (&finish_upload, &finish_receive)
        } else {
            (&finish_receive, &finish_upload)
        };
        first.send(()).unwrap();
        let first_owner = if upload_finishes_first {
            &mut desktop.file_progress
        } else {
            &mut desktop.receive_progress
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !first_owner.as_mut().unwrap().as_mut().unwrap().finish() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            desktop.window_cleanup(),
            WindowCleanup::Pending,
            "the other original native owner still blocks reconnect"
        );
        last.send(()).unwrap();
        while desktop.window_cleanup() == WindowCleanup::Pending {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(desktop.window_cleanup(), WindowCleanup::NotStarted);
    }
}

#[test]
fn download_only_progress_keeps_legacy_access_and_directional_refusals() {
    use crate::file_progress::Error as PanelError;
    let configuration = Configuration::new(Path::new("/usr/bin/false"), ":0", None, 80).unwrap();
    let mut desktop = Desktop::new(configuration);
    desktop.receive_progress = Some(Err(PanelError::InvalidParent));
    assert!(desktop.upload_progress().is_none());
    assert!(matches!(
        desktop.file_progress(),
        Some(Err(PanelError::InvalidParent))
    ));
    desktop.file_progress = Some(Err(PanelError::InvalidDisplay));
    assert!(matches!(
        desktop.upload_progress(),
        Some(Err(PanelError::InvalidDisplay))
    ));
    assert!(matches!(
        desktop.receive_progress(),
        Some(Err(PanelError::InvalidParent))
    ));
}

#[test]
fn input_mapping_must_describe_the_actual_full_native_pixel_image() {
    let bounds = InputBounds::new(DesktopPoint { x: -320, y: -80 }, 320, 240).unwrap();
    let client = InputClient::new(
        InputCredentials {
            session: RemoteSessionId::from_raw(1),
            lease: InputLeaseId::from_raw(2),
            ticket: InputTicketId::from_raw(3),
            view: InputView {
                geometry: DisplayGeometryGeneration::INITIAL,
                viewport: ViewportMappingGeneration::INITIAL,
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
        },
        7,
        bounds,
        Capabilities::default(),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap();
    let display = Display {
        handle: 9,
        geometry: DisplayGeometryGeneration::INITIAL,
        x: -320,
        y: -80,
        pixel_width: 320,
        pixel_height: 240,
        logical_width: 160,
        logical_height: 120,
        scale_numerator: 2,
        scale_denominator: 1,
        rotation: 0,
    };
    let mut viewport = client.viewport();
    let window = crate::viewer_input::Window {
        id: 19,
        width: 320,
        height: 240,
    };
    let full = SurfaceRect::new(0, 0, 320, 240).unwrap();
    let native = viewport.configure(bounds, full).unwrap();
    assert_eq!(check_layout(display, window, &native), Ok(()));
    // Pixel coordinates, not logical/DPI coordinates; moves do not change these.
    let scaled = viewport
        .configure(bounds, SurfaceRect::new(0, 0, 160, 120).unwrap())
        .unwrap();
    assert_eq!(
        check_layout(display, window, &scaled),
        Err(Error::LayoutMismatch)
    );
    let toolbar = viewport
        .configure(bounds, SurfaceRect::new(0, 20, 320, 220).unwrap())
        .unwrap();
    assert_eq!(
        check_layout(display, window, &toolbar),
        Err(Error::LayoutMismatch)
    );
    let cropped = viewport
        .configure(InputBounds::new(bounds.origin(), 160, 120).unwrap(), full)
        .unwrap();
    assert_eq!(
        check_layout(display, window, &cropped),
        Err(Error::LayoutMismatch)
    );
    assert_eq!(
        check_layout(Display { x: 0, ..display }, window, &native),
        Err(Error::LayoutMismatch)
    );
    assert_eq!(
        check_layout(
            display,
            crate::viewer_input::Window {
                width: 640,
                ..window
            },
            &native
        ),
        Err(Error::LayoutMismatch)
    );
    assert_eq!(
        check_layout(
            Display {
                pixel_width: 0,
                ..display
            },
            window,
            &native
        ),
        Err(Error::LayoutMismatch)
    );
}

#[test]
fn fitted_input_matches_rendered_pixels_and_refuses_letterbox_or_stale_layouts() {
    use fr_client::input::viewport::{Error as ViewportError, LocalPoint};
    use fr_media::worker::presentation::{Fit, X11Target};
    for (sw, sh, tw, th) in [
        (320, 240, 160, 160),
        (240, 320, 160, 160),
        (320, 240, 162, 160),
        // The startup clipboard-space policy on a 640x480 screen. The
        // original full image is fitted into this exact reduced target.
        (640, 480, 592, 312),
        (1920, 1080, 592, 312),
    ] {
        let bounds = InputBounds::new(DesktopPoint { x: -320, y: -80 }, sw, sh).unwrap();
        let client = fitted_client(bounds);
        let display = Display {
            handle: 9,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: -320,
            y: -80,
            pixel_width: sw,
            pixel_height: sh,
            logical_width: sw / 2,
            logical_height: sh / 2,
            scale_numerator: 2,
            scale_denominator: 1,
            rotation: 0,
        };
        let window = crate::viewer_input::Window {
            id: 19,
            width: tw,
            height: th,
        };
        let mut viewport = client.viewport();
        let layout = viewport
            .configure(bounds, SurfaceRect::new(0, 0, tw, th).unwrap())
            .unwrap();
        assert_eq!(check_fitted_layout(display, window, &layout), Ok(()));
        assert_eq!(
            check_layout(display, window, &layout),
            Err(Error::LayoutMismatch)
        );
        // A geometry match does not confirm the layout or confer input authority.
        let origin = layout.at(LocalPoint::pixels(0, 0));
        assert_eq!(viewport.map(&origin), Err(ViewportError::Unconfirmed));
        viewport.confirm_layout(&layout).unwrap();
        let fit = Fit::new(sw, sh, X11Target::new(window.id, tw, th).unwrap()).unwrap();
        for y in 0..th {
            for x in 0..tw {
                let actual = viewport.map(&layout.at(LocalPoint::pixels(
                    i32::try_from(x).unwrap(),
                    i32::try_from(y).unwrap(),
                )));
                if (fit.x..fit.x + fit.width).contains(&x)
                    && (fit.y..fit.y + fit.height).contains(&y)
                {
                    let sx = (x - fit.x) * sw / fit.width;
                    let sy = (y - fit.y) * sh / fit.height;
                    assert_eq!(
                        actual,
                        Ok(DesktopPoint {
                            x: -320 + i32::try_from(sx).unwrap(),
                            y: -80 + i32::try_from(sy).unwrap(),
                        })
                    );
                } else {
                    assert_eq!(actual, Err(ViewportError::OutsideImage));
                }
            }
        }
        assert_eq!(
            check_fitted_layout(Display { x: 0, ..display }, window, &layout),
            Err(Error::LayoutMismatch)
        );
        let shifted = viewport
            .configure(bounds, SurfaceRect::new(1, 0, tw, th).unwrap())
            .unwrap();
        assert_eq!(
            check_fitted_layout(display, window, &shifted),
            Err(Error::LayoutMismatch)
        );
        assert_eq!(viewport.map(&origin), Err(ViewportError::Obsolete));
        let crop = InputBounds::new(bounds.origin(), sw / 2, sh / 2).unwrap();
        let cropped = viewport
            .configure(crop, SurfaceRect::new(0, 0, tw, th).unwrap())
            .unwrap();
        assert_eq!(
            check_fitted_layout(display, window, &cropped),
            Err(Error::LayoutMismatch)
        );
    }
}

fn fitted_client(bounds: InputBounds) -> InputClient {
    InputClient::new(
        InputCredentials {
            session: RemoteSessionId::from_raw(1),
            lease: InputLeaseId::from_raw(2),
            ticket: InputTicketId::from_raw(3),
            view: InputView {
                geometry: DisplayGeometryGeneration::INITIAL,
                viewport: ViewportMappingGeneration::INITIAL,
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
        },
        7,
        bounds,
        Capabilities::default(),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap()
}
