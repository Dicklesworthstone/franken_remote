//! Where the picture sits in the window, and which picture pixel a window
//! point is. Platform-neutral so every CI host tests it.

/// A rectangle in window client pixels: `[left, right) x [top, bottom)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The picture's place in a `window`-sized client area: 1:1 and centred
/// whenever it fits (never upscaled, so text stays crisp), otherwise the
/// largest centred aspect-preserving fit. `None` for an empty size.
pub fn fit(picture: (u32, u32), window: (u32, u32)) -> Option<Rect> {
    let (pw, ph) = (u64::from(picture.0), u64::from(picture.1));
    let (ww, wh) = (u64::from(window.0), u64::from(window.1));
    if pw == 0 || ph == 0 || ww == 0 || wh == 0 {
        return None;
    }
    let (w, h) = if pw <= ww && ph <= wh {
        (pw, ph)
    } else if pw * wh <= ph * ww {
        // Height-bound: full height, width scaled (at least one pixel).
        ((pw * wh / ph).max(1), wh)
    } else {
        (ww, (ph * ww / pw).max(1))
    };
    let left = (ww - w) / 2;
    let top = (wh - h) / 2;
    let to_i32 = |v: u64| i32::try_from(v).ok();
    Some(Rect {
        left: to_i32(left)?,
        top: to_i32(top)?,
        right: to_i32(left + w)?,
        bottom: to_i32(top + h)?,
    })
}

/// The picture pixel (fractional) under window client point `(x, y)`, or
/// `None` outside the drawn picture.
pub fn picture_point(dest: Rect, picture: (u32, u32), x: i32, y: i32) -> Option<(f64, f64)> {
    if x < dest.left || y < dest.top || x >= dest.right || y >= dest.bottom {
        return None;
    }
    let width = f64::from(dest.right - dest.left);
    let height = f64::from(dest.bottom - dest.top);
    // The window pixel's centre, scaled into picture pixels.
    let px = (f64::from(x - dest.left) + 0.5) * f64::from(picture.0) / width;
    let py = (f64::from(y - dest.top) + 0.5) * f64::from(picture.1) / height;
    Some((px, py))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_that_fits_is_shown_one_to_one_and_centred() {
        assert_eq!(
            fit((1920, 1080), (1920, 1080)),
            Some(Rect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080
            })
        );
        assert_eq!(
            fit((1280, 720), (1920, 1200)),
            Some(Rect {
                left: 320,
                top: 240,
                right: 1600,
                bottom: 960
            })
        );
    }

    #[test]
    fn a_larger_picture_is_scaled_down_preserving_its_aspect() {
        // 5K 16:9 into a 3:2 window: width-bound.
        assert_eq!(
            fit((5120, 2880), (1920, 1280)),
            Some(Rect {
                left: 0,
                top: 100,
                right: 1920,
                bottom: 1180
            })
        );
        // Tall picture: height-bound.
        assert_eq!(
            fit((1000, 2000), (800, 600)),
            Some(Rect {
                left: 250,
                top: 0,
                right: 550,
                bottom: 600
            })
        );
        assert_eq!(fit((0, 10), (10, 10)), None);
        assert_eq!(fit((10, 10), (10, 0)), None);
    }

    #[test]
    fn window_points_map_to_picture_pixels_only_inside_the_picture() {
        let dest = fit((1280, 720), (1920, 1200)).unwrap();
        // One-to-one: the window pixel's centre is the picture pixel's.
        assert_eq!(picture_point(dest, (1280, 720), 320, 240), Some((0.5, 0.5)));
        assert_eq!(
            picture_point(dest, (1280, 720), 1599, 959),
            Some((1279.5, 719.5))
        );
        // Planted negatives: the borders and the far edge are outside.
        assert_eq!(picture_point(dest, (1280, 720), 319, 240), None);
        assert_eq!(picture_point(dest, (1280, 720), 1600, 500), None);
        assert_eq!(picture_point(dest, (1280, 720), 500, 960), None);
        // Scaled down by 8/3: window pixel 0 covers picture 0..2.67.
        let dest = fit((5120, 2880), (1920, 1280)).unwrap();
        let (x, y) = picture_point(dest, (5120, 2880), 0, 100).unwrap();
        assert!((x - 4.0 / 3.0).abs() < 1e-9 && (y - 4.0 / 3.0).abs() < 1e-9);
    }
}
