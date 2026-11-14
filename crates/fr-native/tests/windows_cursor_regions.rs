//! Exact production cursor policy without a D3D device. These exercise the
//! native adapter's pixels and ownership decisions, not Windows hardware.
#![forbid(unsafe_code)]

#[path = "../src/windows_capture/cursor_region.rs"]
mod cursor_region;

use cursor_region::{Kind, MAX_BYTES, MAX_SIDE, Position, Region, Shape, State};

fn color(width: u32, height: u32, pixel: [u8; 4]) -> Shape {
    Shape::new(
        Kind::Color,
        width,
        height,
        width * 4,
        pixel.repeat((width * height) as usize),
    )
    .unwrap()
}

#[test]
fn color_uses_straight_alpha_and_keeps_the_desktop_opaque() {
    let shape = Shape::new(
        Kind::Color,
        3,
        1,
        12,
        vec![240, 0, 90, 0, 200, 100, 0, 128, 5, 6, 7, 255],
    )
    .unwrap();
    let mut pixels = [10, 20, 30, 255].repeat(3);
    shape.blend_row(0, 0, &mut pixels).unwrap();
    assert_eq!(pixels, [10, 20, 30, 255, 105, 60, 15, 255, 5, 6, 7, 255]);
}

#[test]
fn masked_color_preserves_replace_and_xor_semantics() {
    let shape = Shape::new(
        Kind::MaskedColor,
        3,
        1,
        12,
        vec![7, 8, 9, 0, 255, 15, 240, 255, 0, 0, 0, 255],
    )
    .unwrap();
    let mut pixels = [18, 52, 86, 255].repeat(3);
    shape.blend_row(0, 0, &mut pixels).unwrap();
    assert_eq!(pixels, [7, 8, 9, 255, 237, 59, 166, 255, 18, 52, 86, 255]);
    assert!(Shape::new(Kind::MaskedColor, 1, 1, 4, vec![1, 2, 3, 128]).is_none());
}

#[test]
fn monochrome_handles_black_white_transparent_and_inverting_pixels() {
    // AND followed by XOR: 00 black, 01 white, 10 background, 11 inverse.
    let shape = Shape::new(Kind::Monochrome, 4, 2, 1, vec![0b0011_0000, 0b0101_0000]).unwrap();
    let mut pixels = [18, 52, 86, 255].repeat(4);
    shape.blend_row(0, 0, &mut pixels).unwrap();
    assert_eq!(
        pixels,
        [
            0, 0, 0, 255, 255, 255, 255, 255, 18, 52, 86, 255, 237, 203, 169, 255
        ]
    );
    assert_eq!(shape.region((0, 0), (20, 20)).unwrap().height, 1);
}

#[test]
fn monochrome_clipping_crosses_byte_and_padded_row_boundaries() {
    // 12 x 2 pixels, two AND rows then two XOR rows, padded to four bytes.
    let shape = Shape::new(
        Kind::Monochrome,
        12,
        4,
        4,
        vec![
            0xff,
            0xf0,
            0,
            0,
            0xff,
            0xf0,
            0,
            0,
            0,
            0,
            255,
            255,
            0b0000_0001,
            0b1000_0000,
            255,
            255,
        ],
    )
    .unwrap();
    let mut pixels = [20, 40, 60, 255].repeat(3);
    shape.blend_row(7, 1, &mut pixels).unwrap();
    assert_eq!(
        pixels,
        [235, 215, 195, 255, 235, 215, 195, 255, 20, 40, 60, 255]
    );
}

#[test]
fn native_shape_bounds_refuse_before_any_pixel_indexing() {
    for (kind, width, height, pitch, bytes) in [
        (Kind::Color, 0, 1, 4, vec![0; 4]),
        (Kind::Color, MAX_SIDE + 1, 1, 4, vec![0; 4]),
        (Kind::Color, 1, MAX_SIDE + 1, 4, vec![0; 4]),
        (Kind::Color, 1, 1, 3, vec![0; 4]),
        (Kind::Color, 1, 2, 4, vec![0; 4]),
        (Kind::Color, 1, 2, u32::MAX, vec![0; 8]),
        (Kind::Color, 1, 1, 4, vec![0; MAX_BYTES + 1]),
        (Kind::Monochrome, 1, 1, 1, vec![0; 2]),
        (Kind::Monochrome, 1, u32::MAX, 1, vec![0; 2]),
        (Kind::Monochrome, 9, 2, 1, vec![0; 2]),
    ] {
        assert!(Shape::new(kind, width, height, pitch, bytes).is_none());
    }
    assert!(
        Shape::new(
            Kind::Color,
            MAX_SIDE,
            MAX_SIDE,
            MAX_SIDE * 4,
            vec![0; MAX_BYTES]
        )
        .is_some()
    );
}

#[test]
fn offscreen_pointer_rectangles_are_clipped_without_coordinate_overflow() {
    let shape = color(4, 3, [1, 2, 3, 255]);
    assert_eq!(
        shape.region((-2, -1), (10, 8)),
        Some(Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
            source_x: 2,
            source_y: 1,
        })
    );
    assert_eq!(
        shape.region((9, 7), (10, 8)),
        Some(Region {
            x: 9,
            y: 7,
            width: 1,
            height: 1,
            source_x: 0,
            source_y: 0,
        })
    );
    for position in [
        (i32::MIN, 0),
        (0, i32::MIN),
        (i32::MAX, 0),
        (0, i32::MAX),
        (-4, 0),
        (10, 0),
    ] {
        assert!(shape.region(position, (10, 8)).is_none());
    }
    assert!(shape.region((0, 0), (0, 0)).is_none());
}

#[test]
fn row_blending_rejects_overrun_and_does_not_touch_padding() {
    let shape = color(2, 2, [1, 2, 3, 255]);
    let mut row = [99; 16];
    assert!(shape.blend_row(1, 0, &mut row[..8]).is_none());
    assert!(shape.blend_row(0, 2, &mut row[..4]).is_none());
    assert!(shape.blend_row(u32::MAX, 0, &mut row[..4]).is_none());
    assert!(shape.blend_row(0, 0, &mut row[..3]).is_none());
    assert_eq!(row, [99; 16]);
    shape.blend_row(1, 1, &mut row[..4]).unwrap();
    assert_eq!(&row[..4], &[1, 2, 3, 255]);
    assert_eq!(&row[4..], &[99; 12]);
}

fn show(state: &mut State, position: (i32, i32), shape: Option<Shape>, image: bool) {
    state
        .released(Position::Visible(position), shape, image)
        .unwrap();
}

#[test]
fn only_a_released_dxgi_pointer_update_changes_the_composed_state() {
    let mut state = State::default();
    let empty = state.pointer();
    assert!(!state.has_shape());
    assert!(
        state
            .released(Position::Visible((0, 0)), None, true)
            .is_none()
    );
    assert_eq!(state.pointer(), empty);
    show(&mut state, (3, 2), Some(color(2, 2, [7, 8, 9, 255])), true);
    let visible = state.pointer();
    let (_, region) = state.visible((10, 8)).unwrap();
    assert_eq!((region.x, region.y), (3, 2)); // DXGI top-left; no hotspot subtraction.
    state.drawn(region);
    // Zero LastMouseUpdateTime means no update, not an invisible cursor.
    state.released(Position::Unchanged, None, false).unwrap();
    assert_eq!(state.pointer(), visible);
    assert_eq!(state.take_underlay(), Some(region));
    assert_eq!(state.take_underlay(), None);
    // A shape change at the same coordinates still needs a new picture.
    show(
        &mut state,
        (3, 2),
        Some(color(2, 2, [10, 20, 30, 255])),
        false,
    );
    assert_ne!(state.pointer(), visible);
}

#[test]
fn hidden_or_already_composited_pointer_restores_only_its_old_rectangle() {
    let mut state = State::default();
    show(&mut state, (2, 2), Some(color(2, 2, [7, 8, 9, 255])), true);
    let (_, previous) = state.visible((10, 8)).unwrap();
    state.drawn(previous);
    state.released(Position::Hidden, None, false).unwrap();
    assert!(state.visible((10, 8)).is_none());
    assert_eq!(state.take_underlay(), Some(previous));
    assert!(state.has_shape());
    // The same healthy duplication may show its unchanged shape again.
    show(&mut state, (5, 6), None, false);
    assert!(state.visible((10, 8)).is_some());
}

#[test]
fn a_new_image_discards_the_previous_underlay_without_restoring_old_pixels() {
    let mut state = State::default();
    show(&mut state, (2, 2), Some(color(2, 2, [7, 8, 9, 255])), true);
    let (_, previous) = state.visible((10, 8)).unwrap();
    state.drawn(previous);
    let pointer = state.pointer();
    state.released(Position::Unchanged, None, true).unwrap();
    assert_eq!(state.pointer(), pointer);
    assert!(state.take_underlay().is_none());
    assert!(state.visible((10, 8)).is_some());
}

#[test]
fn access_loss_discards_shape_position_and_underlay_before_recreation() {
    let mut state = State::default();
    show(&mut state, (2, 2), Some(color(2, 2, [7, 8, 9, 255])), true);
    let (_, previous) = state.visible((10, 8)).unwrap();
    state.drawn(previous);
    state.invalidate();
    assert!(!state.has_shape());
    assert!(state.visible((10, 8)).is_none());
    assert!(state.take_underlay().is_none());
    // A replacement's position cannot revive the previous instance's shape.
    assert!(
        state
            .released(Position::Visible((2, 2)), None, true)
            .is_none()
    );
    show(&mut state, (2, 2), Some(color(2, 2, [1, 2, 3, 255])), true);
    assert!(state.visible((10, 8)).is_some());
}

/// Bounded CPU stand-in only for `CopySubresourceRegion`, exercising the real
/// shape blending and state decisions against every background pixel.
fn compose(state: &mut State, desktop: &mut [u8], saved: &mut Vec<u8>) {
    const STRIDE: usize = 8 * 4;
    if let Some(old) = state.take_underlay() {
        for row in 0..old.height as usize {
            let begin = (old.y as usize + row) * STRIDE + old.x as usize * 4;
            let count = old.width as usize * 4;
            desktop[begin..begin + count].copy_from_slice(&saved[row * count..][..count]);
        }
    }
    let Some((shape, region)) = state.visible((8, 6)) else {
        return;
    };
    saved.clear();
    for row in 0..region.height {
        let begin = (region.y + row) as usize * STRIDE + region.x as usize * 4;
        let count = region.width as usize * 4;
        saved.extend_from_slice(&desktop[begin..begin + count]);
        shape
            .blend_row(
                region.source_x,
                region.source_y + row,
                &mut desktop[begin..begin + count],
            )
            .unwrap();
    }
    state.drawn(region);
}

#[test]
fn overlapping_moves_edges_and_hide_leave_no_cursor_trails_or_changed_background() {
    let original: Vec<u8> = (0..8 * 6)
        .flat_map(|pixel| [pixel, pixel + 1, pixel + 2, 255])
        .collect();
    for shape in [
        color(3, 2, [200, 80, 10, 127]),
        Shape::new(Kind::Monochrome, 3, 4, 1, vec![0xe0; 4]).unwrap(),
    ] {
        let mut desktop = original.clone();
        let mut saved = Vec::new();
        let mut state = State::default();
        show(&mut state, (1, 1), Some(shape), true);
        compose(&mut state, &mut desktop, &mut saved);
        for position in [(2, 1), (2, 2), (-1, -1), (7, 5), (8, 6)] {
            show(&mut state, position, None, false);
            compose(&mut state, &mut desktop, &mut saved);
        }
        assert_eq!(desktop, original);
        show(&mut state, (2, 2), None, false);
        compose(&mut state, &mut desktop, &mut saved);
        state.released(Position::Hidden, None, false).unwrap();
        compose(&mut state, &mut desktop, &mut saved);
        assert_eq!(desktop, original);
    }
}

#[test]
fn replacement_image_and_repeated_composition_do_not_mix_background_generations() {
    let mut desktop = [10, 20, 30, 255].repeat(8 * 6);
    let replacement = [90, 80, 70, 255].repeat(8 * 6);
    let mut saved = Vec::new();
    let mut state = State::default();
    show(
        &mut state,
        (2, 2),
        Some(color(3, 2, [200, 100, 40, 127])),
        true,
    );
    compose(&mut state, &mut desktop, &mut saved);
    let once = desktop.clone();
    compose(&mut state, &mut desktop, &mut saved);
    assert_eq!(desktop, once); // A forced IDR must not double-blend the cursor.
    desktop.clone_from(&replacement);
    state.released(Position::Unchanged, None, true).unwrap();
    compose(&mut state, &mut desktop, &mut saved);
    state.released(Position::Hidden, None, false).unwrap();
    compose(&mut state, &mut desktop, &mut saved);
    assert_eq!(desktop, replacement);
}
