//! Initial `AppKit` content size. Input mapping always uses the actual drawable
//! picture after layout; these window points never become remote coordinates.
#![forbid(unsafe_code)]

/// Preserve the remote logical size when it describes the same picture, then
/// fit it inside an optional local window-point ceiling without enlarging it.
/// Products of two u32 values fit in u64, including malformed boundary inputs.
pub(crate) fn points(
    pixels: (u32, u32),
    logical: (u32, u32),
    fit: Option<(u32, u32)>,
) -> Result<(u32, u32), ()> {
    if pixels.0 == 0 || pixels.1 == 0 {
        return Err(());
    }
    let (pw, ph) = (u64::from(pixels.0), u64::from(pixels.1));
    let (lw, lh) = (u64::from(logical.0), u64::from(logical.1));
    let natural = if lw == 0 || lh == 0 || (lw * ph).abs_diff(lh * pw) > pw.max(ph) {
        pixels
    } else {
        logical
    };
    let Some((max_width, max_height)) = fit else {
        return Ok(natural);
    };
    if max_width == 0 || max_height == 0 {
        return Err(());
    }
    if natural.0 <= max_width && natural.1 <= max_height {
        return Ok(natural);
    }
    let (width, height) = (u64::from(natural.0), u64::from(natural.1));
    let (max_width, max_height) = (u64::from(max_width), u64::from(max_height));
    let fitted = if max_width * height <= max_height * width {
        (max_width, (height * max_width / width).max(1))
    } else {
        ((width * max_height / height).max(1), max_height)
    };
    // The constrained dimension is a u32 input; the other is no larger than
    // its own u32 ceiling. Keep checked conversions at this native boundary.
    Ok((
        u32::try_from(fitted.0).map_err(|_| ())?,
        u32::try_from(fitted.1).map_err(|_| ())?,
    ))
}

/// Reserve display choice and file controls in the original content ceiling.
/// The native strip is 44 points high, with 136 points for Change Display,
/// one 230-point cell per file direction, 184 for clipboard and 16 outer padding.
/// A small picture may have horizontal
/// letterboxing so every selected direction retains its local cancel button.
/// These dimensions never participate in remote input coordinates.
pub(crate) fn sharing_points(
    pixels: (u32, u32),
    logical: (u32, u32),
    fit: Option<(u32, u32)>,
    directions: u32,
    clipboard: bool,
) -> Result<(u32, u32), ()> {
    if directions > 2 {
        return Err(());
    }
    let minimum_width = 152 + 230 * directions + u32::from(clipboard) * 184;
    let picture_fit = match fit {
        Some((width, height)) => {
            let height = height.checked_sub(44).ok_or(())?;
            if width < minimum_width || height < 64 {
                return Err(());
            }
            Some((width, height))
        }
        None => None,
    };
    let (width, height) = points(pixels, logical, picture_fit)?;
    Ok((width.max(minimum_width), height.max(64)))
}

/// Reserve the local text editor before establishing a remote viewport. Its
/// 76-point row never opens by resizing a live controlled picture. The old
/// sharing row remains unchanged, including every file/clipboard control.
pub(crate) fn sharing_points_with_text(
    pixels: (u32, u32),
    logical: (u32, u32),
    fit: Option<(u32, u32)>,
    directions: u32,
    clipboard: bool,
    text: bool,
) -> Result<(u32, u32), ()> {
    if !text {
        return sharing_points(pixels, logical, fit, directions, clipboard);
    }
    let picture_and_sharing_fit = match fit {
        Some((width, height)) if width >= 480 => {
            Some((width, height.checked_sub(76).ok_or(())?))
        }
        Some(_) => return Err(()),
        None => None,
    };
    let (width, height) =
        sharing_points(pixels, logical, picture_and_sharing_fit, directions, clipboard)?;
    Ok((width.max(480), height))
}

#[cfg(test)]
mod tests {
    use super::{points, sharing_points, sharing_points_with_text};

    #[test]
    fn text_editor_reserves_both_rows_before_the_picture_is_mapped() {
        for directions in [0, 1, 2] {
            for clipboard in [false, true] {
                let width = (152 + 230 * directions + u32::from(clipboard) * 184).max(480);
                let fitted = sharing_points_with_text(
                    (1920, 1080),
                    (1920, 1080),
                    Some((width, 600)),
                    directions,
                    clipboard,
                    true,
                )
                .unwrap();
                assert_eq!(fitted.0, width);
                assert!(fitted.1 + 120 <= 600);
                assert_eq!(
                    sharing_points_with_text(
                        (64, 64),
                        (64, 64),
                        Some((width, 184)),
                        directions,
                        clipboard,
                        true,
                    ),
                    Ok((width, 64))
                );
                assert!(sharing_points_with_text(
                    (64, 64),
                    (64, 64),
                    Some((width, 183)),
                    directions,
                    clipboard,
                    true,
                )
                .is_err());
            }
        }
        assert!(sharing_points_with_text(
            (64, 64),
            (64, 64),
            Some((479, 600)),
            0,
            false,
            true,
        )
        .is_err());
    }

    #[test]
    fn text_disabled_preserves_the_original_picture_sizing() {
        for fit in [None, Some((800, 600)), Some((152, 108))] {
            assert_eq!(
                sharing_points_with_text((1920, 1080), (1920, 1080), fit, 0, false, false),
                sharing_points((1920, 1080), (1920, 1080), fit, 0, false),
            );
        }
    }

    #[test]
    fn display_and_file_controls_fit_inside_the_original_content_ceiling() {
        for directions in [0, 1, 2] {
            let minimum_width = 152 + 230 * directions;
            assert_eq!(
                sharing_points(
                    (1920, 1080),
                    (1920, 1080),
                    Some((minimum_width - 1, 600)),
                    directions,
                    false
                ),
                Err(())
            );
            assert_eq!(
                sharing_points(
                    (1920, 1080),
                    (1920, 1080),
                    Some((800, 107)),
                    directions,
                    false
                ),
                Err(())
            );
            for pixels in [(1920, 1080), (1080, 1920), (64, 64)] {
                for fit in [(minimum_width, 108), (800, 600), (1280, 720)] {
                    let (width, height) =
                        sharing_points(pixels, pixels, Some(fit), directions, false).unwrap();
                    assert!(width >= minimum_width && width <= fit.0);
                    assert!(height >= 64 && height + 44 <= fit.1);
                }
            }
        }
        assert_eq!(
            sharing_points((1920, 1080), (1920, 1080), Some((800, 600)), 3, false),
            Err(())
        );
        assert_eq!(
            sharing_points((1920, 1080), (1920, 1080), Some((16, 16)), 0, false),
            Err(())
        );
    }

    #[test]
    fn clipboard_off_has_its_own_space_with_every_file_combination() {
        for directions in [0, 1, 2] {
            let minimum = 152 + 230 * directions + 184;
            assert_eq!(
                sharing_points(
                    (64, 64),
                    (64, 64),
                    Some((minimum - 1, 108)),
                    directions,
                    true
                ),
                Err(())
            );
            assert_eq!(
                sharing_points((64, 64), (64, 64), Some((minimum, 108)), directions, true),
                Ok((minimum, 64))
            );
            let fitted = sharing_points(
                (1920, 1080),
                (1920, 1080),
                Some((800, 600)),
                directions,
                true,
            )
            .unwrap();
            assert!(fitted.0 >= minimum && fitted.0 <= 800 && fitted.1 + 44 <= 600);
        }
    }

    #[test]
    fn natural_size_preserves_retina_and_rounding_without_trusting_wrong_aspects() {
        for (pixels, logical, expected) in [
            ((5120, 2880), (2560, 1440), (2560, 1440)),
            ((3000, 2000), (1500, 1000), (1500, 1000)),
            ((1920, 1080), (1920, 1080), (1920, 1080)),
            ((2880, 1800), (1919, 1200), (1919, 1200)),
            ((1920, 1080), (1080, 1920), (1920, 1080)),
            ((1920, 1080), (0, 0), (1920, 1080)),
        ] {
            assert_eq!(points(pixels, logical, None), Ok(expected));
        }
    }

    #[test]
    fn fit_constrains_both_dimensions_and_never_enlarges_the_natural_window() {
        for (pixels, logical, fit, expected) in [
            ((3840, 2160), (1920, 1080), (960, 540), (960, 540)),
            ((1920, 1080), (1920, 1080), (1000, 1000), (1000, 562)),
            ((1080, 1920), (1080, 1920), (1000, 1000), (562, 1000)),
            ((1920, 1080), (1920, 1080), (2000, 500), (888, 500)),
            ((640, 480), (640, 480), (1920, 1080), (640, 480)),
            ((1920, 1080), (0, 0), (16, 16), (16, 9)),
        ] {
            assert_eq!(points(pixels, logical, Some(fit)), Ok(expected));
        }
    }

    #[test]
    fn invalid_zero_sizes_refuse_and_extreme_products_remain_bounded() {
        for pixels in [(0, 1080), (1920, 0), (0, 0)] {
            assert_eq!(points(pixels, (1920, 1080), None), Err(()));
        }
        for fit in [(0, 1080), (1920, 0), (0, 0)] {
            assert_eq!(points((1920, 1080), (1920, 1080), Some(fit)), Err(()));
        }
        assert_eq!(
            points((u32::MAX, u32::MAX), (u32::MAX, u32::MAX), Some((16, 16))),
            Ok((16, 16))
        );
        assert_eq!(points((u32::MAX, 1), (0, 0), Some((16, 16))), Ok((16, 1)));
        assert_eq!(points((1, u32::MAX), (0, 0), Some((16, 16))), Ok((1, 16)));
    }

    #[test]
    fn fit_bounds_hold_for_both_orientations_and_mixed_scale_metadata() {
        let sizes = [
            (16, 16),
            (120, 250),
            (1920, 1080),
            (3000, 2000),
            (u32::MAX, 1),
        ];
        for pixels in sizes {
            for logical in sizes {
                for fit in sizes {
                    let fitted = points(pixels, logical, Some(fit)).unwrap();
                    let natural = points(pixels, logical, None).unwrap();
                    assert!(fitted.0 > 0 && fitted.1 > 0);
                    assert!(fitted.0 <= fit.0 && fitted.1 <= fit.1);
                    assert!(fitted.0 <= natural.0 && fitted.1 <= natural.1);
                }
            }
        }
    }
}
