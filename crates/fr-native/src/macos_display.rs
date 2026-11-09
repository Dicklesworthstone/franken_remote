//! macOS display geometry shared by the capture worker's catalog and the input
//! executor (fr-p1-host-macos-vif). The host's desktop coordinates are the
//! ENCODED pixels of each display: a controller points in them, the capture
//! worker publishes them, and the input executor maps them back to the window
//! server's points. One formula on both sides keeps a click where it was aimed.
//! Pure data, compiled on every target so its tests run in ordinary CI.
#![forbid(unsafe_code)]

use fr_core::{
    input::{DesktopPoint, InputBounds},
    limits::ProtocolLimits,
};

/// One display as Core Graphics describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacDisplay {
    pub id: u32,
    /// Origin in the global desktop, in this display's pixels.
    pub x: i32,
    pub y: i32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub point_width: u32,
    pub point_height: u32,
}

impl MacDisplay {
    /// The size the host encodes. The display's own pixels when they fit
    /// `limits`. Otherwise the largest of a few clean ratios that does: a
    /// clean ratio keeps text crisp, and 6144x3456 becomes 5120x2880 (5/6).
    /// `None` when even half the size does not fit.
    #[must_use]
    pub fn encoded_size(&self, limits: &ProtocolLimits) -> Option<(u32, u32)> {
        const RATIOS: [(u32, u32); 6] = [(1, 1), (5, 6), (4, 5), (3, 4), (2, 3), (1, 2)];
        RATIOS.iter().find_map(|&(n, d)| {
            let (w, h) = (self.pixel_width * n, self.pixel_height * n);
            if !w.is_multiple_of(d) || !h.is_multiple_of(d) {
                return None;
            }
            let (w, h) = (w / d, h / d);
            (w.is_multiple_of(2)
                && h.is_multiple_of(2)
                && limits.validate_coded_dimensions(w, h).is_ok())
            .then_some((w, h))
        })
    }

    /// This display's rectangle in the host's desktop coordinates: its origin
    /// scaled by the encode ratio, its size the encoded size. The capture
    /// catalog publishes exactly this and a controller's grant names it.
    #[must_use]
    pub fn desktop_bounds(&self, limits: &ProtocolLimits) -> Option<InputBounds> {
        let (w, h) = self.encoded_size(limits)?;
        let scale = |v: i32, encoded: u32, native: u32| {
            i32::try_from(i64::from(v) * i64::from(encoded) / i64::from(native)).ok()
        };
        InputBounds::new(
            DesktopPoint {
                x: scale(self.x, w, self.pixel_width)?,
                y: scale(self.y, h, self.pixel_height)?,
            },
            w,
            h,
        )
    }
}

/// Map a desktop point inside `bounds` (one display's `desktop_bounds`) to the
/// window server's global points, given that display's point origin and size.
/// Exact rational scaling: at 1:1 Retina one pixel is half a point, and the
/// fraction is kept (the cursor sits on device pixels, not whole points).
#[must_use]
pub fn to_points(
    bounds: InputBounds,
    origin: (f64, f64),
    point_size: (u32, u32),
    point: DesktopPoint,
) -> (f64, f64) {
    let axis = |p: i32, start: i32, encoded: u32, points: u32, base: f64| {
        base + f64::from(p - start) * f64::from(points) / f64::from(encoded)
    };
    let start = bounds.origin();
    (
        axis(point.x, start.x, bounds.width(), point_size.0, origin.0),
        axis(point.y, start.y, bounds.height(), point_size.1, origin.1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(w: u32, h: u32) -> MacDisplay {
        MacDisplay {
            id: 1,
            x: 0,
            y: 0,
            pixel_width: w,
            pixel_height: h,
            point_width: w / 2,
            point_height: h / 2,
        }
    }

    #[test]
    fn encoded_size_is_native_when_it_fits_else_a_clean_ratio() {
        let limits = ProtocolLimits::ABSOLUTE;
        // 6K Retina exceeds the 16.78 MP cap: 5/6 keeps it crisp.
        assert_eq!(
            display(6144, 3456).encoded_size(&limits),
            Some((5120, 2880))
        );
        assert_eq!(
            display(5120, 2880).encoded_size(&limits),
            Some((5120, 2880))
        );
        assert_eq!(
            display(3456, 2234).encoded_size(&limits),
            Some((3456, 2234))
        );
        // 8K: 5/6, 4/5 and 3/4 all exceed the pixel cap; 2/3 fits.
        assert_eq!(
            display(7680, 4320).encoded_size(&limits),
            Some((5120, 2880))
        );
        assert_eq!(display(16, 16).encoded_size(&limits), Some((16, 16)));
    }

    #[test]
    fn desktop_bounds_scale_the_origin_by_the_encode_ratio() {
        let limits = ProtocolLimits::ABSOLUTE;
        let main = display(6144, 3456);
        assert_eq!(
            main.desktop_bounds(&limits),
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 5120, 2880)
        );
        // A second 6K display to the right: its pixel origin 6144 is 5120
        // encoded pixels, so the two encoded rectangles stay adjacent.
        let right = MacDisplay { x: 6144, ..main };
        assert_eq!(
            right.desktop_bounds(&limits),
            InputBounds::new(DesktopPoint { x: 5120, y: 0 }, 5120, 2880)
        );
        // A display above the main one has a negative origin.
        let above = MacDisplay {
            x: -1000,
            y: -2160,
            ..display(3840, 2160)
        };
        assert_eq!(
            above.desktop_bounds(&limits),
            InputBounds::new(DesktopPoint { x: -1000, y: -2160 }, 3840, 2160)
        );
    }

    #[test]
    fn points_follow_the_encoded_grid_exactly() {
        let limits = ProtocolLimits::ABSOLUTE;
        // 6144x3456 pixels, 3072x1728 points, encoded 5120x2880.
        let bounds = display(6144, 3456).desktop_bounds(&limits).unwrap();
        let map = |x, y| to_points(bounds, (0.0, 0.0), (3072, 1728), DesktopPoint { x, y });
        assert_eq!(map(0, 0), (0.0, 0.0));
        assert_eq!(map(5120, 2880), (3072.0, 1728.0));
        // 5 encoded pixels are 3 points.
        assert_eq!(map(5, 10), (3.0, 6.0));
        assert_eq!(map(2560, 1440), (1536.0, 864.0));
        // 1:1 Retina: odd pixels land on half points.
        let native = display(3456, 2234).desktop_bounds(&limits).unwrap();
        let point = to_points(
            native,
            (0.0, 0.0),
            (1728, 1117),
            DesktopPoint { x: 1, y: 3 },
        );
        assert_eq!(point, (0.5, 1.5));
        // A display placed at (-1728, 100) points keeps its own origin.
        let placed = InputBounds::new(DesktopPoint { x: -3456, y: 200 }, 3456, 2234).unwrap();
        let point = to_points(
            placed,
            (-1728.0, 100.0),
            (1728, 1117),
            DesktopPoint {
                x: -3456 + 10,
                y: 200 + 2234,
            },
        );
        assert_eq!(point, (-1723.0, 1217.0));
    }
}
