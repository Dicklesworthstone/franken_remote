//! Coordinate policy only; fixture grants are not native input authorization.
use super::*;
use fr_client::input::{ClientInstant, InputClient, Policy, viewport::SurfaceRect};
use fr_core::{ids::*, input::*, input_submission::Capabilities, limits::ProtocolLimits};

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
