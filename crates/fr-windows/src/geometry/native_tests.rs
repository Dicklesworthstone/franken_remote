//! Real portable image, input and cursor geometry for native-pixel viewing.
//! No decoded frame, native presentation or freshness witness is fabricated.
use super::*;
use crate::{cursor, cursor_image};

const DISPLAY: (u32, u32) = (5120, 2880);
const WINDOW: (u32, u32) = (1920, 1080);

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
    Rect {
        left,
        top,
        right,
        bottom,
    }
}

fn shown(view: Framing) -> Presentation {
    view.presentation(DISPLAY, DISPLAY, WINDOW).unwrap()
}

#[test]
fn native_pixels_crop_the_retained_texture_and_map_to_that_exact_source_pixel() {
    let presentation = shown(Framing::new(true));
    assert_eq!(presentation.source(), rect(1600, 900, 3520, 1980));
    assert_eq!(presentation.destination, rect(0, 0, 1920, 1080));
    assert_eq!(
        presentation.display_projection(),
        rect(-1600, -900, 3520, 1980)
    );
    assert_eq!(presentation.picture(), DISPLAY);
    assert!(presentation.actual_pixels());
    let mapping = Mapping::for_presentation(presentation, 5, 100);
    assert_eq!(mapping.point(101, 0, 0, false), Ok(Some((1600.5, 900.5))));
    assert_eq!(
        mapping.point(101, 1919, 1079, false),
        Ok(Some((3519.5, 1979.5)))
    );
    assert_eq!(mapping.point(101, -1, 0, false), Ok(None));
    assert_eq!(mapping.point(101, 1920, 0, false), Ok(None));
    assert_eq!(mapping.point(101, 0, 1080, false), Ok(None));
    // A held drag's release remains in the same visible crop, including
    // after the native mouse capture delivered an out-of-window coordinate.
    assert_eq!(
        mapping.point(101, i32::MIN, i32::MAX, true),
        Ok(Some((1600.5, 1979.5)))
    );
}

#[test]
fn small_and_odd_pictures_center_without_scaling_or_losing_an_axis() {
    let framing = Framing::new(true);
    for (display, window, source, destination) in [
        (
            (1365, 767),
            (1400, 800),
            rect(0, 0, 1365, 767),
            rect(17, 16, 1382, 783),
        ),
        (
            (1001, 2401),
            (1200, 801),
            rect(0, 800, 1001, 1601),
            rect(99, 0, 1100, 801),
        ),
        ((1, 1), (3, 3), rect(0, 0, 1, 1), rect(1, 1, 2, 2)),
    ] {
        let picture = framing.presentation(display, display, window).unwrap();
        assert_eq!(picture.source(), source);
        assert_eq!(picture.destination, destination);
        let mapping = Mapping::for_presentation(picture, 4, 10);
        assert_eq!(
            mapping.point(11, destination.left, destination.top, false),
            Ok(Some((
                f64::from(source.left) + 0.5,
                f64::from(source.top) + 0.5
            ))),
        );
    }
}

#[test]
fn panning_reaches_each_image_edge_without_resizing_or_crossing_source_bounds() {
    let mut framing = Framing::new(true);
    for (direction, expected) in [
        (ViewAction::Left, rect(0, 900, 1920, 1980)),
        (ViewAction::Up, rect(0, 0, 1920, 1080)),
        (ViewAction::Right, rect(3200, 0, 5120, 1080)),
        (ViewAction::Down, rect(3200, 1800, 5120, 2880)),
    ] {
        for _ in 0..20 {
            let before = shown(framing);
            framing.apply(direction, before);
            let picture = shown(framing);
            assert_eq!(picture.destination, rect(0, 0, 1920, 1080));
            assert!(picture.source.left >= 0 && picture.source.top >= 0);
            assert!(picture.source.right <= 5120 && picture.source.bottom <= 2880);
        }
        assert_eq!(shown(framing).source(), expected);
        assert!(!framing.apply(direction, shown(framing)), "already at edge");
    }
    assert!(framing.apply(ViewAction::Center, shown(framing)));
    assert_eq!(shown(framing).source(), rect(1600, 900, 3520, 1980));
    assert!(!framing.apply(ViewAction::Center, shown(framing)));
}

#[test]
fn fitting_and_resizing_keep_the_original_full_resolution_request() {
    let mut framing = Framing::new(true);
    framing.apply(ViewAction::Right, shown(framing));
    let panned = shown(framing);
    assert!(framing.apply(ViewAction::Fit, panned));
    assert_eq!(
        shown(framing),
        Presentation::new(DISPLAY, DISPLAY, WINDOW).unwrap()
    );
    for size in [WINDOW, (800, 600), (1920, 1200)] {
        assert_eq!(framing.picture_limit(size), None);
    }
    assert!(
        !framing.apply(ViewAction::Left, shown(framing)),
        "Fit cannot pan"
    );
    assert!(framing.apply(ViewAction::ActualPixels, shown(framing)));
    assert_eq!(shown(framing), panned);
    // A replacement keeps the chosen mode but does not inherit source offsets.
    framing.reset();
    assert_eq!(shown(framing).source(), rect(1600, 900, 3520, 1980));
    assert_eq!(framing.picture_limit(WINDOW), None);
}

#[test]
fn scaled_shared_pictures_do_not_pretend_to_restore_native_detail() {
    let mut framing = Framing::new(true);
    let picture = framing.presentation(DISPLAY, (1582, 888), WINDOW).unwrap();
    assert_eq!(
        picture,
        Presentation::new(DISPLAY, (1582, 888), WINDOW).unwrap()
    );
    assert!(!picture.actual_pixels() && !picture.native_picture());
    assert!(!framing.apply(ViewAction::ActualPixels, picture));
    assert!(!framing.apply(ViewAction::Right, picture));
    assert_eq!(framing.picture_limit(WINDOW), None);
    // Choosing Fit while a scaled shared stream is visible also preserves
    // that preference if a later attempt can provide the native-sized image.
    assert!(!framing.apply(ViewAction::Fit, picture));
    framing.reset();
    assert!(!shown(framing).actual_pixels());
}

#[test]
fn default_viewing_keeps_its_existing_fit_and_cannot_enable_native_controls() {
    let mut framing = Framing::new(false);
    let original = shown(framing);
    assert_eq!(
        original,
        Presentation::new(DISPLAY, DISPLAY, WINDOW).unwrap()
    );
    for action in [
        ViewAction::Fit,
        ViewAction::ActualPixels,
        ViewAction::Left,
        ViewAction::Center,
    ] {
        assert!(!framing.apply(action, original));
        assert_eq!(shown(framing), original);
    }
    assert_eq!(framing.picture_limit(WINDOW), Some(WINDOW));
    assert_eq!(framing.picture_limit((800, 600)), Some((800, 600)));
}

#[test]
fn old_pointer_and_key_messages_cannot_follow_a_pan_or_return_to_the_same_crop() {
    let mut framing = Framing::new(true);
    let centered = shown(framing);
    let old = Mapping::for_presentation(centered, 7, 100);
    assert!(old.admits(101));
    assert!(framing.apply(ViewAction::Right, centered));
    let shifted = shown(framing);
    assert_eq!(shifted.source.left, 3040);
    assert!(!old.same_presentation(shifted, 7));
    let fresh = Mapping::for_presentation(shifted, 7, 120);
    for at in [101, 119, 120] {
        assert!(!fresh.admits(at));
        assert_eq!(fresh.point(at, 10, 10, true), Err(StaleMapping));
    }
    assert_eq!(fresh.point(121, 10, 10, false), Ok(Some((3050.5, 910.5))));
    assert!(framing.apply(ViewAction::Center, shifted));
    let restored = Mapping::for_presentation(shown(framing), 7, 140);
    assert_eq!(shown(framing), centered);
    assert!(!restored.admits(121));
    assert!(!restored.same_presentation(centered, 8));
    assert!(restored.same_presentation(centered, 7));
    assert!(restored.admits(141));
}

#[test]
fn native_and_remote_cursors_share_the_exact_crop_scale_and_hotspot() {
    let presentation = shown(Framing::new(true));
    let shape = cursor::Shape {
        width: 8,
        height: 10,
        hotspot_x: 2,
        hotspot_y: 3,
    };
    let state = cursor::State {
        attempt: 7,
        generation: 3,
        shape_generation: 2,
        frame_after: 4,
        source: DISPLAY,
        visible: true,
        x: 1600,
        y: 900,
        shape,
    };
    let overlay = cursor::placement_for(state, presentation).unwrap();
    assert_eq!(overlay.destination, [-2.0, -3.0, 6.0, 7.0]);
    assert_eq!(overlay.clip, [0.0, 0.0, 1920.0, 1080.0]);
    let native = cursor_image::prepare(
        shape,
        &vec![255; shape.bytes().unwrap()],
        DISPLAY,
        presentation.display_projection(),
    )
    .unwrap();
    assert_eq!(
        native.shape, shape,
        "native cursor is not scaled to the cropped width"
    );
    assert!(cursor::placement_for(cursor::State { x: 100, ..state }, presentation).is_none());
    assert!(
        cursor::placement_for(
            cursor::State {
                source: (1920, 1080),
                ..state
            },
            presentation
        )
        .is_none()
    );
    // The cursor hotspot and a pixel-centre input sample name the same
    // visible source location, irrespective of how far the display was panned.
    let mapped = Mapping::for_presentation(presentation, 7, 10)
        .point(11, 0, 0, false)
        .unwrap()
        .unwrap();
    assert_eq!(mapped, (f64::from(state.x) + 0.5, f64::from(state.y) + 0.5));
}

#[test]
fn invalid_native_dimensions_stay_subject_to_existing_protocol_limits() {
    let framing = Framing::new(true);
    for (display, window) in [
        ((0, 800), WINDOW),
        ((u32::MAX, 1), WINDOW),
        (DISPLAY, (0, 1080)),
        (DISPLAY, (u32::MAX, 2)),
    ] {
        assert!(framing.presentation(display, display, window).is_none());
    }
}
