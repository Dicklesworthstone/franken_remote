//! Scaled-picture placement must agree with the original shared input map.
//! These exercise the portable production geometry, not native presentation.
use super::*;

const DISPLAY: (u32, u32) = (5120, 2880);
const PICTURE: (u32, u32) = (1582, 888);

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
    Rect {
        left,
        top,
        right,
        bottom,
    }
}

#[test]
fn canonical_rounding_keeps_rendered_edges_inside_the_shared_input_rectangle() {
    let presentation = Presentation::new(DISPLAY, PICTURE, PICTURE).unwrap();
    // The host's even crop is slightly wider than the selected display's
    // exact aspect. The shared input viewport reserves two columns per side.
    assert_eq!(presentation.input, rect(2, 0, 1580, 888));
    assert_eq!(presentation.destination, rect(2, 0, 1580, 888));
    assert_eq!(presentation.picture(), PICTURE);
    assert_eq!(presentation.window(), PICTURE);
    let mapping = Mapping::for_presentation(presentation, 7, 1_000);
    assert_eq!(mapping.point(1_001, 2, 0, false), Ok(Some((2.5, 0.5))));
    assert_eq!(
        mapping.point(1_001, 1579, 887, false),
        Ok(Some((1579.5, 887.5)))
    );
    assert_eq!(mapping.point(1_001, 1, 0, false), Ok(None));
    assert_eq!(mapping.point(1_001, 1580, 0, false), Ok(None));
    assert_eq!(mapping.point(1_001, 2, 888, false), Ok(None));

    // A timely drag release outside the image still lands inside the granted
    // image; the old full-picture map incorrectly produced x=0.5 here.
    assert_eq!(
        mapping.point(1_001, i32::MIN, i32::MIN, true),
        Ok(Some((2.5, 0.5)))
    );
    assert_eq!(
        mapping.point(1_001, i32::MAX, i32::MAX, true),
        Ok(Some((1579.5, 887.5)))
    );
}

#[test]
fn a_larger_window_preserves_crisp_size_without_reusing_encoded_aspect() {
    let presentation = Presentation::new(DISPLAY, PICTURE, (1920, 1200)).unwrap();
    assert_eq!(presentation.destination, rect(171, 156, 1749, 1044));
    let mapping = Mapping::for_presentation(presentation, 7, 1_000);
    assert_eq!(mapping.point(1_001, 171, 156, false), Ok(Some((2.5, 0.5))));
    assert_eq!(
        mapping.point(1_001, 1748, 1043, false),
        Ok(Some((1579.5, 887.5)))
    );
    // Both sides of the outer letterbox remain outside the image.
    assert_eq!(mapping.point(1_001, 170, 156, false), Ok(None));
    assert_eq!(mapping.point(1_001, 1749, 156, false), Ok(None));
}

#[test]
fn shared_smaller_pictures_keep_the_same_source_aspect_and_input_edges() {
    // A shared source may retain another viewer's canonical smaller picture.
    let presentation = Presentation::new(DISPLAY, (640, 360), (1920, 1080)).unwrap();
    assert_eq!(presentation.destination, rect(640, 360, 1280, 720));
    assert_eq!(presentation.input, rect(0, 0, 640, 360));
    let mapping = Mapping::for_presentation(presentation, 4, 80);
    assert_eq!(mapping.point(81, 640, 360, false), Ok(Some((0.5, 0.5))));
    assert_eq!(
        mapping.point(81, 1279, 719, false),
        Ok(Some((639.5, 359.5)))
    );
}

#[test]
fn odd_original_displays_and_asymmetric_bars_keep_exact_input_bounds() {
    let display = (1365, 767);
    let native = Presentation::new(display, display, (1400, 800)).unwrap();
    assert_eq!(native.input, rect(0, 0, 1365, 767));
    assert_eq!(native.destination, rect(17, 16, 1382, 783));
    let native = Mapping::for_presentation(native, 3, 10);
    assert_eq!(native.point(11, 17, 16, false), Ok(Some((0.5, 0.5))));

    let scaled = Presentation::new(display, (640, 358), (640, 358)).unwrap();
    assert_eq!(scaled.input, rect(1, 0, 638, 358));
    assert_eq!(scaled.destination, rect(1, 0, 638, 358));
    let scaled = Mapping::for_presentation(scaled, 3, 10);
    assert_eq!(scaled.point(11, 1, 0, false), Ok(Some((1.5, 0.5))));
    assert_eq!(scaled.point(11, 637, 357, false), Ok(Some((637.5, 357.5))));
    assert_eq!(scaled.point(11, 638, 357, false), Ok(None));
}

#[test]
fn portrait_and_smaller_windows_keep_mapping_inside_the_shared_input_area() {
    let presentation = Presentation::new((2880, 5120), (888, 1578), (600, 900)).unwrap();
    assert_eq!(presentation.input, rect(0, 0, 887, 1578));
    assert_eq!(presentation.destination, rect(47, 0, 553, 900));
    let mapping = Mapping::for_presentation(presentation, 3, 10);
    for (x, y) in [(47, 0), (552, 0), (47, 899), (552, 899), (300, 450)] {
        let (px, py) = mapping.point(11, x, y, false).unwrap().unwrap();
        assert!(px > 0.0 && px < 887.0);
        assert!(py > 0.0 && py < 1578.0);
        // Undo the shared core's original-display viewport, independently of
        // the encoded crop's aspect ratio. It must name the displayed point.
        let host_x = px * 2880.0 / 887.0;
        let host_y = py * 5120.0 / 1578.0;
        let shown_x = (f64::from(x - 47) + 0.5) * 2880.0 / 506.0;
        let shown_y = (f64::from(y) + 0.5) * 5120.0 / 900.0;
        assert!((host_x - shown_x).abs() < 0.000_001);
        assert!((host_y - shown_y).abs() < 0.000_001);
    }
}

#[test]
fn repaints_keep_the_mapping_time_but_replacement_attempts_cannot_reuse_it() {
    let presentation = Presentation::new(DISPLAY, PICTURE, PICTURE).unwrap();
    let mapping = Mapping::for_presentation(presentation, 7, 1_000);
    assert!(mapping.same_presentation(presentation, 7));
    assert!(!mapping.same_presentation(presentation, 8));
    assert!(!mapping.same_presentation(presentation, 0));
    assert!(mapping.point(1_000, 100, 100, false).is_err());
    assert!(mapping.point(1_001, 100, 100, false).unwrap().is_some());

    let replacement = Mapping::for_presentation(presentation, 8, 1_020);
    assert!(replacement.point(1_019, 100, 100, false).is_err());
    assert!(replacement.point(1_020, 100, 100, false).is_err());
    assert!(replacement.point(1_021, 100, 100, false).unwrap().is_some());
    let unbound = Mapping::for_presentation(presentation, 0, 0);
    assert!(unbound.point(1, 100, 100, false).is_err());

    // Equal aspect and equal resulting rectangles do not make a changed
    // original display the same mapping.
    let other = Presentation::new((2560, 1440), PICTURE, PICTURE).unwrap();
    assert_eq!(presentation.destination, other.destination);
    assert!(!mapping.same_presentation(other, 7));
}

#[test]
fn resizing_and_returning_to_old_dimensions_require_new_mapping_time() {
    let original = Presentation::new(DISPLAY, PICTURE, PICTURE).unwrap();
    let resized = Presentation::new(DISPLAY, PICTURE, (800, 600)).unwrap();
    let first = Mapping::for_presentation(original, 7, 100);
    assert!(!first.same_presentation(resized, 7));
    let second = Mapping::for_presentation(resized, 7, 120);
    assert!(second.point(119, 200, 200, true).is_err());
    assert!(second.point(120, 200, 200, true).is_err());
    assert!(second.point(121, 200, 200, true).unwrap().is_some());
    let restored = Mapping::for_presentation(original, 7, 140);
    assert!(restored.point(101, 200, 200, true).is_err());
}

#[test]
fn invalid_or_unbounded_geometry_refuses_before_a_mapping_exists() {
    for (display, picture, window) in [
        ((0, 2880), PICTURE, PICTURE),
        (DISPLAY, (1582, 0), PICTURE),
        (DISPLAY, PICTURE, (0, 888)),
        ((u32::MAX, 2880), PICTURE, PICTURE),
        (DISPLAY, (u32::MAX, 888), PICTURE),
        (DISPLAY, PICTURE, (u32::MAX, u32::MAX)),
        (DISPLAY, (5122, 2880), (5122, 2880)),
        (DISPLAY, (5120, 2882), (5120, 2882)),
    ] {
        assert!(Presentation::new(display, picture, window).is_none());
    }
    // Shared Viewport refuses a zero-width fit rather than fabricating a
    // one-pixel area; the renderer must refuse that same geometry.
    assert_eq!(display_area((1, 8192), (1, 1)), None);
}
