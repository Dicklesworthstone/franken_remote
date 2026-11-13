//! Production sampling and cursor composition; no native display claim.
use super::*;
use crate::{CursorOverlay, OverlayPlacement};
use fr_core::limits::LimitOverrides;
use fr_media::worker::overlay::Update;

fn mapped(display: (u32, u32), window: (u32, u32)) -> MappedX11Target {
    MappedX11Target::new(
        X11Target::new(1, window.0, window.1).unwrap(),
        display.0,
        display.1,
    )
    .unwrap()
}

fn assert_samples(source: &BgraFrame, output: &BgraFrame, fit: Fit) {
    // Independent integer oracle: encoded dimensions select samples only;
    // placement and bars come from the original full display, not the codec.
    for y in 0..output.height() {
        for x in 0..output.width() {
            let at = usize::try_from((y * output.width() + x) * 4).unwrap();
            let actual = &output.pixels()[at..at + 4];
            if x >= fit.x && x < fit.x + fit.width && y >= fit.y && y < fit.y + fit.height {
                let sx = u64::from(x - fit.x) * u64::from(source.width()) / u64::from(fit.width);
                let sy = u64::from(y - fit.y) * u64::from(source.height()) / u64::from(fit.height);
                let sample = usize::try_from((sy * u64::from(source.width()) + sx) * 4).unwrap();
                assert_eq!(actual, &source.pixels()[sample..sample + 4]);
            } else {
                assert_eq!(actual, &[0, 0, 0, 255]);
            }
        }
    }
}

#[test]
fn a_scaled_picture_fills_its_original_display_rectangle_with_bars() {
    let target = mapped((5120, 2880), (1000, 750));
    assert_eq!(
        target.fit(),
        Fit {
            x: 0,
            y: 94,
            width: 1000,
            height: 562
        }
    );
    // The legacy fitted mode cannot represent a target taller than its
    // decoded picture. The mapped mode knows those extra rows are bars.
    assert!(
        FittedFrame::for_viewport(1000, 562, target.target(), ProtocolLimits::ABSOLUTE).is_err()
    );
    let mut fitted = FittedFrame::for_mapped_viewport(1000, 562, target, ProtocolLimits::ABSOLUTE)
        .unwrap()
        .unwrap();
    let source = super::tests::pattern(1000, 562, 31);
    assert_samples(&source, fitted.render(&source).unwrap(), target.fit());
    let ptr = fitted.output_mut().pixels().as_ptr();
    let bytes = fitted.retained_bytes();
    let next = super::tests::pattern(1000, 562, 99);
    assert_eq!(fitted.render(&next).unwrap().pixels().as_ptr(), ptr);
    assert_eq!(fitted.retained_bytes(), bytes);
    assert_eq!(bytes, 1000 * 750 * 4);
}

#[test]
fn even_codec_rounding_cannot_shift_the_display_cursor_or_letterbox() {
    let target = mapped((5120, 2880), (1582, 888));
    let fit = Fit {
        x: 2,
        y: 0,
        width: 1578,
        height: 888,
    };
    assert_eq!(target.fit(), fit);
    let mut fitted = FittedFrame::for_mapped_viewport(1582, 888, target, ProtocolLimits::ABSOLUTE)
        .unwrap()
        .unwrap();
    let source = super::tests::pattern(1582, 888, 31);
    assert_samples(&source, fitted.render(&source).unwrap(), fit);
    let mut cursor = CursorOverlay::new();
    let placement = OverlayPlacement::Fitted {
        fit,
        source_width: 5120,
        source_height: 2880,
    };
    cursor
        .apply(&Update::Shape {
            x: 2560,
            y: 1440,
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: &[255, 0, 255, 255],
        })
        .unwrap();
    cursor
        .composite_fresh(fitted.output_mut(), placement)
        .unwrap();
    let at = (444_usize * 1582 + 791) * 4;
    assert_eq!(
        &fitted.output_mut().pixels()[at..at + 4],
        &[255, 0, 255, 255]
    );
    // Moving to the last display pixel stays inside the same content rectangle.
    cursor.apply(&Update::Move { x: 5119, y: 2879 }).unwrap();
    cursor.recomposite(fitted.output_mut(), placement).unwrap();
    for x in [0_usize, 1, 1580, 1581] {
        let at = (887 * 1582 + x) * 4;
        assert_eq!(&fitted.output_mut().pixels()[at..at + 4], &[0, 0, 0, 255]);
    }
    cursor.apply(&Update::Hidden).unwrap();
    cursor.recomposite(fitted.output_mut(), placement).unwrap();
    assert_samples(&source, fitted.output_mut(), fit);
}

#[test]
fn a_smaller_shared_picture_uses_bounded_sampling_in_the_same_display_rectangle() {
    let target = mapped((5120, 2880), (1000, 750));
    let mut fitted = FittedFrame::for_mapped_viewport(640, 360, target, ProtocolLimits::ABSOLUTE)
        .unwrap()
        .unwrap();
    let source = super::tests::pattern(640, 360, 83);
    assert_samples(&source, fitted.render(&source).unwrap(), target.fit());
    assert_eq!(fitted.retained_bytes(), 1000 * 750 * 4);
    let source = super::tests::pattern(640, 362, 83);
    assert_eq!(
        fitted.render(&source).unwrap_err(),
        NativeError::GeometryChanged
    );
}

#[test]
fn mapped_identity_skips_copy_but_invalid_geometry_and_limits_never_do() {
    let target = mapped((5120, 2880), (1920, 1080));
    assert!(
        FittedFrame::for_mapped_viewport(1920, 1080, target, ProtocolLimits::ABSOLUTE)
            .unwrap()
            .is_none()
    );
    for (w, h) in [
        (1920, 1082),
        (1920, 1078),
        (640, 362),
        (0, 0),
        (u32::MAX, 1080),
    ] {
        assert!(FittedFrame::for_mapped_viewport(w, h, target, ProtocolLimits::ABSOLUTE).is_err());
    }
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_coded_pixels: Some(1_000_000),
        ..Default::default()
    })
    .unwrap();
    // Both the decoded source and the locally owned output have independent caps.
    assert!(FittedFrame::for_mapped_viewport(1920, 1080, target, limits).is_err());
    assert!(FittedFrame::for_mapped_viewport(640, 360, target, limits).is_err());
    // Smaller negotiated codec limits do not erase the original display coordinates.
    let target = mapped((5120, 2880), (1000, 750));
    assert!(FittedFrame::for_mapped_viewport(1000, 562, target, limits).is_ok());
}
