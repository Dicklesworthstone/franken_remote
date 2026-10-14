//! CPU ownership and geometry regressions; no native display qualification.
use super::super::cursor_overlay::{CursorOverlay, Placement};
use super::*;
use fr_core::limits::LimitOverrides;
use fr_media::worker::overlay::Update;

#[test]
fn native_size_fitting_needs_no_output_owner_at_workstation_resolutions() {
    for (width, height) in [(16, 16), (1920, 1080), (2560, 1440), (3840, 2160)] {
        let target = X11Target::new(1, width, height).unwrap();
        assert!(
            FittedFrame::for_viewport(width, height, target, ProtocolLimits::ABSOLUTE)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn identity_selection_never_bypasses_source_geometry_or_negotiated_limits() {
    let dimensions = ProtocolLimits::with_overrides(LimitOverrides {
        max_dimension_pixels: Some(100),
        ..Default::default()
    })
    .unwrap();
    let pixels = ProtocolLimits::with_overrides(LimitOverrides {
        max_coded_pixels: Some(10_000),
        ..Default::default()
    })
    .unwrap();
    for (w, h, limits) in [(128, 128, dimensions), (128, 128, pixels)] {
        let target = X11Target::new(1, w, h).unwrap();
        assert!(matches!(
            FittedFrame::for_viewport(w, h, target, limits),
            Err(NativeError::InvalidConfiguration)
        ));
    }
    let target = X11Target::new(1, 32, 32).unwrap();
    for (w, h) in [(0, 0), (31, 32), (32, 31), (8, 8), (u32::MAX, u32::MAX)] {
        assert!(FittedFrame::for_viewport(w, h, target, ProtocolLimits::ABSOLUTE).is_err());
    }
    assert!(FittedFrame::for_viewport(16, 16, target, ProtocolLimits::ABSOLUTE).is_err());
}

#[test]
fn equal_pixel_count_and_one_matching_axis_are_not_identity_layouts() {
    for (sw, sh, tw, th) in [(64, 32, 32, 64), (64, 32, 64, 16), (64, 32, 32, 32)] {
        let target = X11Target::new(1, tw, th).unwrap();
        let expected = FittedFrame::new(sw, sh, target, ProtocolLimits::ABSOLUTE);
        let selected = FittedFrame::for_viewport(sw, sh, target, ProtocolLimits::ABSOLUTE);
        match (expected, selected) {
            (Ok(expected), Ok(Some(selected))) => {
                assert_eq!(selected.placement(), expected.placement());
                assert_eq!(selected.retained_bytes(), expected.retained_bytes());
            }
            (Err(expected), Err(actual)) => assert_eq!(expected, actual),
            pair => panic!("non-identity viewport bypassed fitting: {pair:?}"),
        }
    }
}

#[test]
fn native_size_retains_cursor_save_under_and_recycles_the_original_picture() {
    let target = X11Target::new(1, 64, 32).unwrap();
    assert!(
        FittedFrame::for_viewport(64, 32, target, ProtocolLimits::ABSOLUTE)
            .unwrap()
            .is_none()
    );
    // The worker's existing None path composites into the owned decoded
    // picture, then moves it into `last` for idle cursor updates.
    let source: Vec<_> = (0..64_u32 * 32).flat_map(u32::to_le_bytes).collect();
    let mut picture = BgraFrame::new(64, 32, source.clone(), &ProtocolLimits::ABSOLUTE).unwrap();
    let pointer = picture.pixels().as_ptr();
    let mut cursor = CursorOverlay::new();
    cursor
        .apply(&Update::Shape {
            x: 4,
            y: 8,
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: &[255; 16],
        })
        .unwrap();
    cursor
        .composite_fresh(&mut picture, Placement::Identity)
        .unwrap();
    assert_ne!(picture.pixels(), source);
    let mut last = Some(picture);
    for (x, y) in [(24, 12), (63, 31), (-1, -1)] {
        cursor.apply(&Update::Move { x, y }).unwrap();
        let retained = last.as_mut().unwrap();
        cursor.recomposite(retained, Placement::Identity).unwrap();
        assert_eq!(retained.pixels().as_ptr(), pointer);
    }
    cursor.apply(&Update::Hidden).unwrap();
    cursor
        .recomposite(last.as_mut().unwrap(), Placement::Identity)
        .unwrap();
    let retired = last.take().unwrap();
    assert_eq!(retired.pixels(), source);
    assert_eq!(retired.pixels().as_ptr(), pointer);
}

#[test]
fn selected_downscale_keeps_its_single_retained_output_and_opaque_bars() {
    let target = X11Target::new(1, 32, 32).unwrap();
    let mut selected = FittedFrame::for_viewport(64, 32, target, ProtocolLimits::ABSOLUTE)
        .unwrap()
        .unwrap();
    assert_eq!(
        selected.placement(),
        Fit {
            x: 0,
            y: 8,
            width: 32,
            height: 16
        }
    );
    let source = super::tests::pattern(64, 32, 17);
    let ptr = selected.render(&source).unwrap().pixels().as_ptr();
    for value in [31, 47, 83] {
        let pixels = super::tests::pattern(64, 32, value);
        assert_eq!(selected.render_mut(&pixels).unwrap().pixels().as_ptr(), ptr);
        assert_eq!(selected.output_mut().pixels().as_ptr(), ptr);
        assert_eq!(selected.retained_bytes(), 32 * 32 * 4);
        assert_eq!(&selected.output_mut().pixels()[..4], &[0, 0, 0, 255]);
    }
}
