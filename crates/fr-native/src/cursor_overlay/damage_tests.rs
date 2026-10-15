//! Production compositing and exact damage, without a native display claim.
use super::*;
const NO_DAMAGE: &[[u32; 4]] = &[];

fn regions(overlay: &mut CursorOverlay, frame: &mut BgraFrame) -> CursorDamage {
    overlay
        .recomposite_regions(frame, Placement::Identity)
        .unwrap()
}

#[test]
fn move_hide_and_offscreen_updates_report_only_old_and_new_cursor_rectangles() {
    let mut f = frame(64, 64, GREY);
    let mut overlay = CursorOverlay::new();
    let rgba = MAGENTA.repeat(16);
    overlay.apply(&shape(3, 4, &rgba, 4, 1)).unwrap();
    overlay
        .composite_fresh(&mut f, Placement::Identity)
        .unwrap();
    let retained = overlay.retained_bytes();
    overlay.apply(&Update::Move { x: 62, y: 63 }).unwrap();
    assert_eq!(
        regions(&mut overlay, &mut f).regions(),
        &[[2, 3, 4, 4], [61, 62, 3, 2]]
    );
    assert_eq!(pixel(&f, 2, 3), GREY);
    assert_eq!(pixel(&f, 63, 63), MAGENTA);
    overlay
        .apply(&Update::Move {
            x: i32::MIN,
            y: i32::MAX,
        })
        .unwrap();
    assert_eq!(regions(&mut overlay, &mut f).regions(), &[[61, 62, 3, 2]]);
    assert_eq!(f.pixels(), frame(64, 64, GREY).pixels());
    overlay.apply(&Update::Hidden).unwrap();
    assert_eq!(regions(&mut overlay, &mut f).regions(), NO_DAMAGE);
    overlay.apply(&Update::Move { x: 10, y: 11 }).unwrap();
    assert_eq!(regions(&mut overlay, &mut f).regions(), &[[9, 10, 4, 4]]);
    overlay.apply(&Update::Hidden).unwrap();
    assert_eq!(regions(&mut overlay, &mut f).regions(), &[[9, 10, 4, 4]]);
    assert_eq!(
        overlay.retained_bytes(),
        retained,
        "moves reuse the bounded storage"
    );
    assert_eq!(f.pixels(), frame(64, 64, GREY).pixels());
}

#[test]
fn unchanged_effective_position_does_no_work_but_a_new_shape_repaints_once() {
    let mut f = frame(32, 32, GREY);
    let mut overlay = CursorOverlay::new();
    let rgba = MAGENTA.repeat(16);
    overlay.apply(&shape(8, 8, &rgba, 4, 0)).unwrap();
    overlay
        .composite_fresh(&mut f, Placement::Identity)
        .unwrap();
    for _ in 0..100 {
        overlay.apply(&Update::Move { x: 8, y: 8 }).unwrap();
        assert_eq!(regions(&mut overlay, &mut f).regions(), NO_DAMAGE);
    }
    // Same bounds, different straight-alpha pixels: deduplicate the two
    // destination areas, not the shape change or its save-under restoration.
    let translucent = [255, 0, 0, 128].repeat(16);
    overlay.apply(&shape(8, 8, &translucent, 4, 0)).unwrap();
    assert_eq!(regions(&mut overlay, &mut f).regions(), &[[8, 8, 4, 4]]);
    assert_eq!(pixel(&f, 8, 8), [32, 32, 160, 255]);
    assert_eq!(regions(&mut overlay, &mut f).regions(), NO_DAMAGE);
    overlay.apply(&Update::Hidden).unwrap();
    regions(&mut overlay, &mut f);
    assert_eq!(f.pixels(), frame(32, 32, GREY).pixels());
}

#[test]
fn a_fresh_picture_is_fully_composited_and_never_restores_the_previous_background() {
    let mut overlay = CursorOverlay::new();
    let rgba = MAGENTA.repeat(16);
    overlay.apply(&shape(10, 10, &rgba, 4, 0)).unwrap();
    for background in [GREY, [11, 22, 33, 255], [101, 102, 103, 255]] {
        let mut f = frame(32, 32, background);
        overlay.apply(&Update::Move { x: 10, y: 10 }).unwrap();
        overlay
            .composite_fresh(&mut f, Placement::Identity)
            .unwrap();
        assert_eq!(pixel(&f, 10, 10), MAGENTA);
        assert_eq!(regions(&mut overlay, &mut f).regions(), NO_DAMAGE);
        overlay.apply(&Update::Hidden).unwrap();
        assert_eq!(regions(&mut overlay, &mut f).regions(), &[[10, 10, 4, 4]]);
        assert_eq!(f.pixels(), frame(32, 32, background).pixels());
    }
}

#[test]
fn a_different_retained_layout_is_refused_before_restoring_any_saved_pixel() {
    let mut overlay = CursorOverlay::new();
    let rgba = MAGENTA.repeat(16);
    overlay.apply(&shape(60, 60, &rgba, 4, 0)).unwrap();
    let mut original = frame(64, 64, GREY);
    overlay
        .composite_fresh(&mut original, Placement::Identity)
        .unwrap();
    overlay.apply(&Update::Hidden).unwrap();
    let mut small = frame(16, 16, [11, 22, 33, 255]);
    assert_eq!(
        overlay.recomposite_regions(&mut small, Placement::Identity),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(small.pixels(), frame(16, 16, [11, 22, 33, 255]).pixels());
    let alternate = Placement::Fitted {
        fit: Fit {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        },
        source_width: 128,
        source_height: 128,
    };
    let shown = original.pixels().to_vec();
    assert_eq!(
        overlay.recomposite_regions(&mut original, alternate),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(original.pixels(), shown);
    // Refusal did not consume saved custody: the original layout can still
    // restore its exact background. An explicit fresh picture can rebase it.
    assert_eq!(
        regions(&mut overlay, &mut original).regions(),
        &[[60, 60, 4, 4]]
    );
    assert_eq!(original.pixels(), frame(64, 64, GREY).pixels());
    overlay
        .composite_fresh(&mut small, Placement::Identity)
        .unwrap();
    assert_eq!(regions(&mut overlay, &mut small).regions(), NO_DAMAGE);
}

#[test]
fn invalid_fresh_placement_keeps_old_save_under_and_pixels_intact_even_when_hidden() {
    let mut overlay = CursorOverlay::new();
    let rgba = MAGENTA.repeat(16);
    let mut f = frame(32, 32, GREY);
    overlay.apply(&shape(10, 10, &rgba, 4, 0)).unwrap();
    overlay
        .composite_fresh(&mut f, Placement::Identity)
        .unwrap();
    overlay.apply(&Update::Hidden).unwrap();
    let shown = f.pixels().to_vec();
    for (fit, source_width, error) in [
        (
            Fit {
                x: 0,
                y: 0,
                width: 0,
                height: 16,
            },
            64,
            NativeError::InvalidConfiguration,
        ),
        (
            Fit {
                x: 0,
                y: 0,
                width: 32,
                height: 16,
            },
            0,
            NativeError::InvalidConfiguration,
        ),
        (
            Fit {
                x: u32::MAX,
                y: 0,
                width: 32,
                height: 16,
            },
            64,
            NativeError::GeometryChanged,
        ),
    ] {
        let bad = Placement::Fitted {
            fit,
            source_width,
            source_height: 32,
        };
        assert_eq!(overlay.composite_fresh(&mut f, bad), Err(error));
        assert_eq!(f.pixels(), shown);
    }
    assert_eq!(regions(&mut overlay, &mut f).regions(), &[[10, 10, 4, 4]]);
    assert_eq!(f.pixels(), frame(32, 32, GREY).pixels());
}

#[test]
fn fitted_damage_is_in_destination_pixels_and_never_in_letterbox_bars() {
    let placement = Placement::Fitted {
        fit: Fit {
            x: 0,
            y: 8,
            width: 32,
            height: 16,
        },
        source_width: 64,
        source_height: 32,
    };
    let mut f = frame(32, 32, GREY);
    let mut overlay = CursorOverlay::new();
    let rgba = MAGENTA.repeat(4);
    overlay.apply(&shape(40, 10, &rgba, 2, 0)).unwrap();
    overlay.composite_fresh(&mut f, placement).unwrap();
    // Quantization maps both positions to the same destination rectangle.
    overlay.apply(&Update::Move { x: 41, y: 11 }).unwrap();
    assert_eq!(
        overlay
            .recomposite_regions(&mut f, placement)
            .unwrap()
            .regions(),
        NO_DAMAGE
    );
    overlay.apply(&Update::Move { x: 0, y: -2 }).unwrap();
    assert_eq!(
        overlay
            .recomposite_regions(&mut f, placement)
            .unwrap()
            .regions(),
        &[[20, 13, 2, 2], [0, 8, 2, 1]]
    );
    for y in (0..8).chain(24..32) {
        for x in 0..32 {
            assert_eq!(pixel(&f, x, y), GREY);
        }
    }
}

#[test]
fn identical_destination_bounds_do_not_hide_a_changed_clipped_source_offset() {
    let mut rgba = vec![0; 32 * 32 * 4];
    for (index, p) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        p.copy_from_slice(&[u8::try_from(index % 256).unwrap(), 0, 0, 255]);
    }
    let mut overlay = CursorOverlay::new();
    let mut f = frame(16, 16, GREY);
    overlay.apply(&shape(0, 0, &rgba, 32, 0)).unwrap();
    overlay
        .composite_fresh(&mut f, Placement::Identity)
        .unwrap();
    let before = pixel(&f, 0, 0);
    overlay.apply(&Update::Move { x: -1, y: 0 }).unwrap();
    assert_eq!(regions(&mut overlay, &mut f).regions(), &[[0, 0, 16, 16]]);
    assert_ne!(
        pixel(&f, 0, 0),
        before,
        "source offset is part of the no-op check"
    );
}

#[test]
fn damage_covers_every_changed_pixel_through_overlaps_edges_and_extreme_positions() {
    let coords = [
        i32::MIN,
        -256,
        -4,
        -1,
        0,
        1,
        15,
        16,
        30,
        31,
        32,
        255,
        i32::MAX,
    ];
    for size in [1_u16, 4, 16, 256] {
        let rgba = [255, 17, 89, 128].repeat(usize::from(size) * usize::from(size));
        let mut overlay = CursorOverlay::new();
        let mut f = frame(32, 32, GREY);
        overlay.apply(&shape(4, 7, &rgba, size, size / 2)).unwrap();
        overlay
            .composite_fresh(&mut f, Placement::Identity)
            .unwrap();
        for x in coords {
            for y in coords {
                let before = f.pixels().to_vec();
                overlay.apply(&Update::Move { x, y }).unwrap();
                let damage = regions(&mut overlay, &mut f);
                assert!(damage.regions().len() <= 2);
                for &[rx, ry, w, h] in damage.regions() {
                    assert!(w > 0 && h > 0 && w <= 256 && h <= 256);
                    assert!(rx < 32 && ry < 32 && w <= 32 - rx && h <= 32 - ry);
                }
                let mut oracle = CursorOverlay::new();
                oracle.apply(&shape(x, y, &rgba, size, size / 2)).unwrap();
                let mut expected = frame(32, 32, GREY);
                oracle
                    .composite_fresh(&mut expected, Placement::Identity)
                    .unwrap();
                assert_eq!(
                    f.pixels(),
                    expected.pixels(),
                    "restoration/blend must match a fresh composite"
                );
                for (index, (old, new)) in before
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(f.pixels().as_chunks::<4>().0.iter())
                    .enumerate()
                {
                    if old != new {
                        let px = u32::try_from(index % 32).unwrap();
                        let py = u32::try_from(index / 32).unwrap();
                        assert!(damage.regions().iter().any(|&[rx, ry, w, h]| rx <= px
                            && px < rx + w
                            && ry <= py
                            && py < ry + h));
                    }
                }
            }
        }
        assert!(overlay.retained_bytes() <= 2 * 256 * 256 * 4);
    }
}
