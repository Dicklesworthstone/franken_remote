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

/// The largest picture new viewer windows on the main screen show 1:1, and
/// how a later window size maps to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowBox {
    /// The visible screen below the title bar and the strips, in pixels.
    pub pixels: (u32, u32),
    /// Whole pixels per point.
    pub scale: u32,
    /// Points of native strips below the picture (sharing, text, playback).
    pub reserved: u32,
}

impl WindowBox {
    /// The box for a window whose content (strips included) is at most `fit`
    /// points, never beyond the screen's: the `--fit` ceiling, then each
    /// resized window. A `full_screen` window's `fit` was measured in full
    /// screen, where no title bar, menu bar or Dock takes room, so the
    /// windowed screen ceiling does not apply to it. `None` when less than 64
    /// pixels remain on an axis.
    #[must_use]
    pub fn within(self, fit: Option<(u32, u32)>, full_screen: bool) -> Option<(u32, u32)> {
        let ceiling = if full_screen {
            (u32::MAX, u32::MAX)
        } else {
            self.pixels
        };
        let (width, height) = match fit {
            Some((width, height)) => (
                ceiling.0.min(width.saturating_mul(self.scale)),
                ceiling.1.min(
                    height
                        .saturating_sub(self.reserved)
                        .saturating_mul(self.scale),
                ),
            ),
            None => self.pixels,
        };
        (width.min(height) >= 64).then_some((width, height))
    }
}

/// The window points that show a scaled picture 1:1 on a screen with `scale`
/// pixels per point, or `None` when the expected picture is the display's
/// native size (the remote's logical size then applies). `expected` is what a
/// scaling host sends for the viewer's stated box; a host that cannot scale
/// sends native pixels instead, which the renderer fits into the same window.
pub(crate) fn scaled_picture_points(
    native: (u32, u32),
    expected: (u32, u32),
    scale: u32,
) -> Option<(u32, u32)> {
    (expected != native && scale > 0 && expected.0 > 0 && expected.1 > 0)
        .then(|| (expected.0.div_ceil(scale), expected.1.div_ceil(scale)))
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
        Some((width, height)) if width >= 480 => Some((width, height.checked_sub(76).ok_or(())?)),
        Some(_) => return Err(()),
        None => None,
    };
    let (width, height) = sharing_points(
        pixels,
        logical,
        picture_and_sharing_fit,
        directions,
        clipboard,
    )?;
    Ok((width.max(480), height))
}

/// The explicit file-selection button has its own 144-point slot beside the
/// existing direction/progress controls. Keep both rows inside the same fit
/// ceiling; never enlarge a controlled picture when selection is requested.
pub(crate) fn sharing_points_with_selections(
    pixels: (u32, u32),
    logical: (u32, u32),
    fit: Option<(u32, u32)>,
    directions: u32,
    clipboard: bool,
    text: bool,
    selections: bool,
) -> Result<(u32, u32), ()> {
    if directions > 2 || (selections && directions == 0) {
        return Err(());
    }
    let minimum = 152 + 230 * directions + u32::from(clipboard) * 184 + u32::from(selections) * 144;
    if fit.is_some_and(|(width, _)| width < minimum) {
        return Err(());
    }
    let (width, height) =
        sharing_points_with_text(pixels, logical, fit, directions, clipboard, text)?;
    Ok((width.max(minimum), height))
}

/// Reserve the optional 44-point playback row before establishing a viewport.
/// The local volume and mute controls never resize an existing remote picture.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sharing_points_with_audio(
    pixels: (u32, u32),
    logical: (u32, u32),
    fit: Option<(u32, u32)>,
    directions: u32,
    clipboard: bool,
    text: bool,
    selections: bool,
    audio: bool,
) -> Result<(u32, u32), ()> {
    let fit = if audio {
        match fit {
            Some((width, height)) if width >= 320 => {
                Some((width, height.checked_sub(44).ok_or(())?))
            }
            Some(_) => return Err(()),
            None => None,
        }
    } else {
        fit
    };
    let (width, height) = sharing_points_with_selections(
        pixels, logical, fit, directions, clipboard, text, selections,
    )?;
    Ok((if audio { width.max(320) } else { width }, height))
}

#[cfg(test)]
mod tests {
    use super::{
        WindowBox, points, scaled_picture_points, sharing_points, sharing_points_with_audio,
        sharing_points_with_selections, sharing_points_with_text,
    };

    #[test]
    fn playback_controls_reserve_space_before_every_selected_viewport() {
        for directions in 0..=2 {
            for clipboard in [false, true] {
                for text in [false, true] {
                    for selections in [false, true] {
                        if selections && directions == 0 {
                            continue;
                        }
                        let width = (152
                            + directions * 230
                            + u32::from(clipboard) * 184
                            + u32::from(selections) * 144)
                            .max(if text { 480 } else { 320 });
                        let reserved = 88 + u32::from(text) * 76;
                        let height = reserved + 64;
                        assert_eq!(
                            sharing_points_with_audio(
                                (64, 64),
                                (64, 64),
                                Some((width, height)),
                                directions,
                                clipboard,
                                text,
                                selections,
                                true
                            ),
                            Ok((width, 64))
                        );
                        assert!(
                            sharing_points_with_audio(
                                (64, 64),
                                (64, 64),
                                Some((width, height - 1)),
                                directions,
                                clipboard,
                                text,
                                selections,
                                true
                            )
                            .is_err()
                        );
                        assert!(
                            sharing_points_with_audio(
                                (64, 64),
                                (64, 64),
                                Some((width - 1, height)),
                                directions,
                                clipboard,
                                text,
                                selections,
                                true
                            )
                            .is_err()
                        );
                        let fit = Some((width, 600));
                        let (_, picture_height) = sharing_points_with_audio(
                            (1920, 1080),
                            (1920, 1080),
                            fit,
                            directions,
                            clipboard,
                            text,
                            selections,
                            true,
                        )
                        .unwrap();
                        assert!(picture_height + reserved <= 600);
                        assert_eq!(
                            sharing_points_with_audio(
                                (1920, 1080),
                                (1920, 1080),
                                fit,
                                directions,
                                clipboard,
                                text,
                                selections,
                                false
                            ),
                            sharing_points_with_selections(
                                (1920, 1080),
                                (1920, 1080),
                                fit,
                                directions,
                                clipboard,
                                text,
                                selections
                            )
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_box_follows_the_window_up_to_the_screen() {
        // mac-mini-old: 1920x1080 at 1x, Dock shown, view-only.
        let screen = WindowBox {
            pixels: (1920, 933),
            scale: 1,
            reserved: 44,
        };
        assert_eq!(screen.within(None, false), Some((1920, 933)));
        // A window resized to 1200x744 points shows 1200x700 below the strip.
        assert_eq!(screen.within(Some((1200, 744)), false), Some((1200, 700)));
        // Larger than the screen: the screen's box.
        assert_eq!(screen.within(Some((4000, 3000)), false), Some((1920, 933)));
        // In full screen the whole 1920x1080 content is the window's: the
        // picture is 1920x1036 above the strip, past the windowed ceiling.
        assert_eq!(screen.within(Some((1920, 1080)), true), Some((1920, 1036)));
        // A full-screen window smaller than the screen (a notch or a menu bar
        // kept shown) states what it measured.
        assert_eq!(screen.within(Some((1920, 1050)), true), Some((1920, 1006)));
        // A Retina screen with the text composer reserved.
        let retina = WindowBox {
            pixels: (3024, 1700),
            scale: 2,
            reserved: 120,
        };
        assert_eq!(retina.within(Some((800, 620)), false), Some((1600, 1000)));
        assert_eq!(retina.within(Some((1512, 982)), true), Some((3024, 1724)));
        // Too small for a picture: no box, native pictures.
        assert_eq!(screen.within(Some((1200, 107)), false), None);
        assert_eq!(screen.within(Some((63, 700)), false), None);
        assert_eq!(screen.within(Some((1200, 40)), false), None);
        assert_eq!(screen.within(Some((1200, 40)), true), None);
        assert_eq!(
            WindowBox {
                pixels: (1920, 933),
                scale: 2,
                reserved: 44,
            }
            .within(Some((u32::MAX, 744)), false),
            Some((1920, 933))
        );
    }

    #[test]
    fn a_scaled_picture_opens_at_one_point_per_pixel_group() {
        // A 5K catalog entry scaled for a 1x screen: one point per pixel.
        assert_eq!(
            scaled_picture_points((5120, 2880), (1754, 986), 1),
            Some((1754, 986))
        );
        // For a 2x screen: half, and never smaller than the picture.
        assert_eq!(
            scaled_picture_points((5120, 2880), (3024, 1700), 2),
            Some((1512, 850))
        );
        // A native picture keeps the remote's logical size.
        assert_eq!(scaled_picture_points((1920, 1080), (1920, 1080), 2), None);
        assert_eq!(scaled_picture_points((5120, 2880), (1920, 1080), 0), None);
    }

    #[test]
    fn native_selection_button_and_both_cancel_controls_fit_inside_the_same_ceiling() {
        for directions in [1, 2] {
            for clipboard in [false, true] {
                let width = 152 + 230 * directions + u32::from(clipboard) * 184 + 144;
                for pixels in [(64, 64), (1920, 1080), (1080, 1920)] {
                    let fitted = sharing_points_with_selections(
                        pixels,
                        pixels,
                        Some((width, 600)),
                        directions,
                        clipboard,
                        true,
                        true,
                    )
                    .unwrap();
                    assert_eq!(fitted.0, width);
                    assert!(fitted.1 >= 64 && fitted.1 + 120 <= 600);
                    assert!(
                        sharing_points_with_selections(
                            pixels,
                            pixels,
                            Some((width - 1, 600)),
                            directions,
                            clipboard,
                            true,
                            true
                        )
                        .is_err()
                    );
                }
                assert_eq!(
                    sharing_points_with_selections(
                        (64, 64),
                        (64, 64),
                        Some((width, 184)),
                        directions,
                        clipboard,
                        true,
                        true
                    ),
                    Ok((width, 64))
                );
            }
        }
        assert!(
            sharing_points_with_selections((64, 64), (64, 64), None, 0, false, true, true).is_err()
        );
        assert!(
            sharing_points_with_selections((64, 64), (64, 64), None, u32::MAX, false, true, true)
                .is_err()
        );
        assert_eq!(
            sharing_points_with_selections((64, 64), (64, 64), None, 1, false, true, false),
            sharing_points_with_text((64, 64), (64, 64), None, 1, false, true)
        );
    }

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
                assert!(
                    sharing_points_with_text(
                        (64, 64),
                        (64, 64),
                        Some((width, 183)),
                        directions,
                        clipboard,
                        true,
                    )
                    .is_err()
                );
            }
        }
        assert!(
            sharing_points_with_text((64, 64), (64, 64), Some((479, 600)), 0, false, true,)
                .is_err()
        );
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
