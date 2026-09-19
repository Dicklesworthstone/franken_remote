use super::*;
fn pattern(w: u32, h: u32, offset: u8) -> BgraFrame {
    let mut pixels = Vec::new();
    for y in 0..h {
        for x in 0..w {
            pixels.extend_from_slice(&[
                u8::try_from(x % 256).unwrap(),
                u8::try_from(y % 256).unwrap(),
                offset,
                255,
            ]);
        }
    }
    BgraFrame::new(w, h, pixels, &ProtocolLimits::ABSOLUTE).unwrap()
}
#[test]
fn fit_uses_one_allocation_and_preserves_exact_integer_mapping() {
    let mut fitted = FittedFrame::new(
        64,
        32,
        X11Target::new(1, 32, 32).unwrap(),
        ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    assert_eq!(
        fitted.placement(),
        Fit {
            x: 0,
            y: 8,
            width: 32,
            height: 16
        }
    );
    assert_eq!(fitted.retained_bytes(), 32 * 32 * 4);
    let initial_ptr = fitted
        .render(&pattern(64, 32, 17))
        .unwrap()
        .pixels()
        .as_ptr();
    for offset in [17, 53, 199] {
        let output = fitted.render(&pattern(64, 32, offset)).unwrap();
        assert_eq!(output.pixels().as_ptr(), initial_ptr);
        assert_eq!((output.width(), output.height()), (32, 32));
        for y in 0..32_usize {
            for x in 0..32_usize {
                let pixel = &output.pixels()[(y * 32 + x) * 4..(y * 32 + x + 1) * 4];
                if (8..24).contains(&y) {
                    assert_eq!(
                        pixel,
                        &[
                            u8::try_from(x * 2).unwrap(),
                            u8::try_from((y - 8) * 2).unwrap(),
                            offset,
                            255
                        ]
                    );
                } else {
                    assert_eq!(pixel, &[0, 0, 0, 255]);
                }
            }
        }
    }
}
#[test]
fn changed_source_and_upscales_are_refused_without_reallocating() {
    let mut fitted = FittedFrame::new(
        64,
        32,
        X11Target::new(1, 32, 16).unwrap(),
        ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    assert!(matches!(
        fitted.render(&pattern(32, 32, 1)),
        Err(NativeError::GeometryChanged)
    ));
    assert_eq!(fitted.retained_bytes(), 32 * 16 * 4);
    assert!(
        FittedFrame::new(
            32,
            32,
            X11Target::new(1, 64, 32).unwrap(),
            ProtocolLimits::ABSOLUTE
        )
        .is_err()
    );
    assert!(
        FittedFrame::new(
            u32::MAX,
            u32::MAX,
            X11Target::new(1, 32, 32).unwrap(),
            ProtocolLimits::ABSOLUTE
        )
        .is_err()
    );
}
#[test]
fn odd_letterbox_remainder_and_last_pixel_are_never_stale() {
    let target = X11Target::new(1, 34, 32).unwrap();
    let mut fitted = FittedFrame::new(64, 32, target, ProtocolLimits::ABSOLUTE).unwrap();
    assert_eq!(
        fitted.placement(),
        Fit {
            x: 0,
            y: 7,
            width: 34,
            height: 17
        }
    );
    let output = fitted.render(&pattern(64, 32, 201)).unwrap();
    assert_eq!(
        &output.pixels()[(23 * 34 + 33) * 4..(23 * 34 + 34) * 4],
        &[62, 30, 201, 255]
    );
    for row in [0, 6, 24, 31] {
        assert!(
            output.pixels()[row * 34 * 4..(row + 1) * 34 * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [0, 0, 0, 255])
        );
    }
}
