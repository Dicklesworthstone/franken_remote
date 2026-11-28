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

/// One picture's placement and its corresponding shared-core input area.
///
/// The original display supplies the aspect ratio. The encoded picture only
/// supplies samples and the upper bound for the existing no-upscale policy.
/// The shared input owner separately fits that display into encoded-picture
/// coordinates, so its rounding/letterbox offsets must also be retained here.
/// This value is geometry, not a session, visibility or input-authority token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    pub destination: Rect,
    /// Samples from the retained decoded texture. Local panning never asks
    /// the host to capture a different region or changes the frame's age.
    source: Rect,
    input: Rect,
    /// The complete original display's projection, including pixels outside
    /// the window. Both remote and native cursors use this scale; only the
    /// destination rectangle is visible/hit-testable.
    display_projection: Rect,
    display: (u32, u32),
    picture: (u32, u32),
    window: (u32, u32),
    actual_pixels: bool,
}

impl Presentation {
    /// Construct only from the current attempt's selected display and the
    /// retained picture's own dimensions. Negotiation validates the canonical
    /// picture rule separately; invalid/oversized local geometry refuses here.
    pub fn new(display: (u32, u32), picture: (u32, u32), window: (u32, u32)) -> Option<Self> {
        let limits = fr_core::limits::ProtocolLimits::ABSOLUTE;
        for (width, height) in [display, picture, window] {
            limits.validate_coded_dimensions(width, height).ok()?;
        }
        if picture.0 > display.0 || picture.1 > display.1 {
            return None;
        }
        let input = display_area(display, picture)?;
        let bounded = (picture.0.min(window.0), picture.1.min(window.1));
        let fitted = display_area(display, bounded)?;
        let width = fitted.right.abs_diff(fitted.left);
        let height = fitted.bottom.abs_diff(fitted.top);
        let left = (window.0 - width) / 2;
        let top = (window.1 - height) / 2;
        let destination = Rect {
            left: i32::try_from(left).ok()?,
            top: i32::try_from(top).ok()?,
            right: i32::try_from(left.checked_add(width)?).ok()?,
            bottom: i32::try_from(top.checked_add(height)?).ok()?,
        };
        Some(Self {
            destination,
            source: Rect {
                left: 0,
                top: 0,
                right: i32::try_from(picture.0).ok()?,
                bottom: i32::try_from(picture.1).ok()?,
            },
            input,
            display_projection: destination,
            display,
            picture,
            window,
            actual_pixels: false,
        })
    }

    /// One decoded pixel per physical client pixel. Only a native-sized
    /// picture can establish this mode; a shared publisher's smaller picture
    /// must remain explicitly fitted instead of implying restored detail.
    fn native(display: (u32, u32), window: (u32, u32), origin: Option<(u32, u32)>) -> Option<Self> {
        let mut result = Self::new(display, display, window)?;
        let width = display.0.min(window.0);
        let height = display.1.min(window.1);
        let limit = (display.0 - width, display.1 - height);
        let origin = origin.unwrap_or((limit.0 / 2, limit.1 / 2));
        let (x, y) = (origin.0.min(limit.0), origin.1.min(limit.1));
        let left = i32::try_from((window.0 - width) / 2).ok()?;
        let top = i32::try_from((window.1 - height) / 2).ok()?;
        let (x, y, width, height) = (
            i32::try_from(x).ok()?,
            i32::try_from(y).ok()?,
            i32::try_from(width).ok()?,
            i32::try_from(height).ok()?,
        );
        result.destination = Rect {
            left,
            top,
            right: left.checked_add(width)?,
            bottom: top.checked_add(height)?,
        };
        result.source = Rect {
            left: x,
            top: y,
            right: x.checked_add(width)?,
            bottom: y.checked_add(height)?,
        };
        // In a native-sized picture the shared input viewport is the whole
        // display. A visible pixel therefore maps straight to its crop offset.
        result.input = result.source;
        result.display_projection = Rect {
            left: left.checked_sub(x)?,
            top: top.checked_sub(y)?,
            right: left
                .checked_sub(x)?
                .checked_add(i32::try_from(display.0).ok()?)?,
            bottom: top
                .checked_sub(y)?
                .checked_add(i32::try_from(display.1).ok()?)?,
        };
        result.actual_pixels = true;
        Some(result)
    }

    pub const fn source(self) -> Rect {
        self.source
    }

    pub const fn display_projection(self) -> Rect {
        self.display_projection
    }

    pub const fn actual_pixels(self) -> bool {
        self.actual_pixels
    }

    pub fn native_picture(self) -> bool {
        self.display == self.picture
    }

    pub const fn picture(self) -> (u32, u32) {
        self.picture
    }

    pub const fn display(self) -> (u32, u32) {
        self.display
    }

    pub const fn window(self) -> (u32, u32) {
        self.window
    }
}

/// Explicit local commands. These never become remote input shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewAction {
    Fit,
    ActualPixels,
    Left,
    Right,
    Up,
    Down,
    Center,
}

/// One window's local viewing preference. Requesting a native-sized picture
/// is immutable for the launch; fitting that retained image later cannot
/// shrink a host request or authorize a different capture scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framing {
    native_requested: bool,
    actual_pixels: bool,
    origin: Option<(u32, u32)>,
}

impl Framing {
    pub const fn new(native_requested: bool) -> Self {
        Self {
            native_requested,
            actual_pixels: native_requested,
            origin: None,
        }
    }

    pub const fn native_requested(self) -> bool {
        self.native_requested
    }

    /// This exact policy is used for initial and replacement native sessions.
    pub const fn picture_limit(self, window: (u32, u32)) -> Option<(u32, u32)> {
        if self.native_requested {
            None
        } else {
            Some(window)
        }
    }

    /// A different display/attempt starts centered; local coordinates never
    /// follow the old source. The user's Fit/Actual preference is retained.
    pub fn reset(&mut self) {
        self.origin = None;
    }

    pub fn presentation(
        self,
        display: (u32, u32),
        picture: (u32, u32),
        window: (u32, u32),
    ) -> Option<Presentation> {
        if self.native_requested && self.actual_pixels && picture == display {
            Presentation::native(display, window, self.origin)
        } else {
            Presentation::new(display, picture, window)
        }
    }

    /// Change only the checked, currently presented picture. Pan by three
    /// quarters of the visible extent, clamping at the original image edge.
    /// `false` means no visible change (or this launch/mode cannot pan).
    pub fn apply(&mut self, action: ViewAction, current: Presentation) -> bool {
        if !self.native_requested {
            return false;
        }
        match action {
            ViewAction::Fit => self.actual_pixels = false,
            ViewAction::ActualPixels if current.native_picture() => self.actual_pixels = true,
            ViewAction::ActualPixels => return false,
            _ if !current.actual_pixels => return false,
            ViewAction::Center => self.origin = None,
            direction => {
                let width = current.source.right.abs_diff(current.source.left);
                let height = current.source.bottom.abs_diff(current.source.top);
                let (Ok(mut x), Ok(mut y)) = (
                    u32::try_from(current.source.left),
                    u32::try_from(current.source.top),
                ) else {
                    return false;
                };
                let step = ((width * 3 / 4).max(1), (height * 3 / 4).max(1));
                match direction {
                    ViewAction::Left => x = x.saturating_sub(step.0),
                    ViewAction::Right => {
                        x = x.saturating_add(step.0).min(current.picture.0 - width);
                    }
                    ViewAction::Up => y = y.saturating_sub(step.1),
                    ViewAction::Down => {
                        y = y.saturating_add(step.1).min(current.picture.1 - height);
                    }
                    _ => unreachable!("non-pan commands handled above"),
                }
                self.origin = Some((x, y));
            }
        }
        self.presentation(current.display, current.picture, current.window) != Some(current)
    }
}

/// The same integer aspect fit used by the shared input `Viewport`. A vanishing
/// axis refuses instead of inventing a one-pixel input area. All operands were
/// bounded before these products.
fn display_area(display: (u32, u32), area: (u32, u32)) -> Option<Rect> {
    let (sw, sh) = (u64::from(display.0), u64::from(display.1));
    let (aw, ah) = (u64::from(area.0), u64::from(area.1));
    if sw == 0 || sh == 0 || aw == 0 || ah == 0 {
        return None;
    }
    let (width, height) = if aw * sh <= ah * sw {
        (aw, aw * sh / sw)
    } else {
        (ah * sw / sh, ah)
    };
    if width == 0 || height == 0 {
        return None;
    }
    let left = (aw - width) / 2;
    let top = (ah - height) / 2;
    Some(Rect {
        left: i32::try_from(left).ok()?,
        top: i32::try_from(top).ok()?,
        right: i32::try_from(left + width).ok()?,
        bottom: i32::try_from(top + height).ok()?,
    })
}

/// One actually presented window-to-picture mapping. A resize invalidates it
/// before native work; presenting the retained picture establishes a new one.
/// Native messages queued under the earlier placement never use the new one.
/// No Debug: positions are private input state.
pub struct Mapping {
    presentation: Presentation,
    attempt: u64,
    /// Suspend-inclusive native tick, sampled after presentation succeeded.
    presented_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleMapping;

impl Mapping {
    #[cfg(test)]
    pub fn new(dest: Rect, picture: (u32, u32), presented_at: u64) -> Self {
        Self::for_presentation(
            Presentation {
                destination: dest,
                source: Rect {
                    left: 0,
                    top: 0,
                    right: i32::try_from(picture.0).expect("bounded test picture"),
                    bottom: i32::try_from(picture.1).expect("bounded test picture"),
                },
                input: Rect {
                    left: 0,
                    top: 0,
                    right: i32::try_from(picture.0).expect("bounded test picture"),
                    bottom: i32::try_from(picture.1).expect("bounded test picture"),
                },
                display_projection: dest,
                display: picture,
                picture,
                window: (
                    u32::try_from(dest.right).expect("bounded test window"),
                    u32::try_from(dest.bottom).expect("bounded test window"),
                ),
                actual_pixels: false,
            },
            1,
            presented_at,
        )
    }

    /// Retain the exact rendered rectangle and the original display's input
    /// rectangle inside the encoded picture. Encoded rounding cannot retarget
    /// a pointer to a letterbox column in the shared input owner.
    pub const fn for_presentation(
        presentation: Presentation,
        attempt: u64,
        presented_at: u64,
    ) -> Self {
        Self {
            presentation,
            attempt,
            presented_at,
        }
    }

    /// A repaint with unchanged geometry keeps its original input timestamp.
    /// Equal dimensions on a new connection attempt still need a fresh native
    /// timestamp, so queued coordinates cannot cross into that successor.
    pub fn same_presentation(&self, presentation: Presentation, attempt: u64) -> bool {
        attempt != 0 && self.attempt == attempt && self.presentation == presentation
    }

    /// Applies to queued keys too when an explicit local crop changes: the
    /// key cannot outlive the local view in which Windows originally queued it.
    pub fn admits(&self, message_at: u64) -> bool {
        self.attempt != 0 && message_at > self.presented_at
    }

    /// Ordinary pictures with the same placement do not restart input's local
    /// mapping lifetime. Returning to the same size after invalidation does.
    #[cfg(test)]
    pub fn same_layout(&self, dest: Rect, picture: (u32, u32)) -> bool {
        self.presentation.destination == dest && self.presentation.picture == picture
    }

    /// `Err` is a stale mapping, `Ok(None)` an ordinary letterbox/outside point.
    /// Refuse the presentation's own tick too: coarse native timestamps cannot
    /// establish whether a same-tick event preceded or followed the change.
    pub fn point(
        &self,
        message_at: u64,
        x: i32,
        y: i32,
        dragging: bool,
    ) -> Result<Option<(f64, f64)>, StaleMapping> {
        if !self.admits(message_at) {
            return Err(StaleMapping);
        }
        let dest = self.presentation.destination;
        let (x, y) = if dragging {
            (
                x.clamp(dest.left, dest.right - 1),
                y.clamp(dest.top, dest.bottom - 1),
            )
        } else {
            (x, y)
        };
        let input = self.presentation.input;
        Ok(picture_point(
            dest,
            (
                input.right.abs_diff(input.left),
                input.bottom.abs_diff(input.top),
            ),
            x,
            y,
        )
        .map(|(px, py)| (f64::from(input.left) + px, f64::from(input.top) + py)))
    }
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

/// Whether a window whose client area became `client` should reopen the
/// session to ask for a new picture: the host fits its `display` into the
/// stated box (aspect kept, never larger than native), so a new box only
/// helps when that fit differs from the `picture` arriving now. A box under
/// 64 pixels on an axis gets native pictures anyway (frd's offer rule), and
/// before the first picture there is nothing to compare. Within 2 pixels is
/// the host's own rounding (even encoder sizes), not a different picture.
pub fn reopen_for(display: (u32, u32), picture: (u32, u32), client: (u32, u32)) -> bool {
    if client.0 < 64 || client.1 < 64 || display.0 == 0 || display.1 == 0 || picture.0 == 0 {
        return false;
    }
    let Some(rect) = fit(display, client) else {
        return false;
    };
    let wanted = (
        rect.right.abs_diff(rect.left),
        rect.bottom.abs_diff(rect.top),
    );
    wanted.0.abs_diff(picture.0) > 2 || wanted.1.abs_diff(picture.1) > 2
}

/// What one picture proves about the host: `Some(true)` when it scaled (a
/// host that does not scale always sends the display's native size),
/// `Some(false)` when it sent native pixels although the stated box was
/// smaller (it ignored the box), `None` while the display fits the box and
/// either answer would look the same.
pub fn host_scales(display: (u32, u32), stated: (u32, u32), picture: (u32, u32)) -> Option<bool> {
    if display.0 == 0 || display.1 == 0 || picture.0 == 0 || picture.1 == 0 {
        return None;
    }
    if picture != display {
        return Some(true);
    }
    let boxed = stated.0 >= 64 && stated.1 >= 64;
    (boxed && (stated.0 < display.0 || stated.1 < display.1)).then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_coordinates_cannot_use_a_newly_presented_resize_mapping() {
        let picture = (1280, 720);
        let original = Mapping::new(fit(picture, (1920, 1200)).unwrap(), picture, 1_000);
        assert_eq!(original.point(1_001, 320, 240, false), Ok(Some((0.5, 0.5))));
        let resized = Mapping::new(fit(picture, (1280, 720)).unwrap(), picture, 1_020);
        // (320,240) names a different desktop pixel after resize. An old
        // native message must not be interpreted at that new location.
        assert!(resized.point(1_019, 320, 240, false).is_err());
        assert!(resized.point(1_020, 320, 240, false).is_err());
        assert_eq!(
            resized.point(1_021, 320, 240, false),
            Ok(Some((320.5, 240.5)))
        );
        // Returning to the old geometry is still a fresh mapping lifetime.
        let restored = Mapping::new(fit(picture, (1920, 1200)).unwrap(), picture, 1_040);
        assert!(restored.point(1_001, 320, 240, false).is_err());
    }

    #[test]
    fn a_timely_drag_clamps_for_release_but_letterbox_clicks_are_not_stale() {
        let picture = (1280, 720);
        let dest = fit(picture, (1920, 1200)).unwrap();
        let mapping = Mapping::new(dest, picture, 1_000);
        assert_eq!(mapping.point(1_001, 0, 0, false), Ok(None));
        assert_eq!(mapping.point(1_001, 0, 0, true), Ok(Some((0.5, 0.5))));
        assert_eq!(
            mapping.point(1_001, i32::MAX, i32::MAX, true),
            Ok(Some((1279.5, 719.5)))
        );
        assert!(mapping.point(999, 0, 0, true).is_err());
        assert!(mapping.same_layout(dest, picture));
        assert!(!mapping.same_layout(dest, (640, 360)));
        assert!(!mapping.same_layout(fit(picture, (1280, 720)).unwrap(), picture));
    }

    #[test]
    fn a_picture_proves_whether_the_host_scales_only_when_it_can() {
        // A 5K Mac host fitting its display into the window: scales.
        assert_eq!(
            host_scales((5120, 2880), (2008, 1232), (2008, 1128)),
            Some(true)
        );
        // An older host (or a Linux one) sending native 5K into that box.
        assert_eq!(
            host_scales((5120, 2880), (2008, 1232), (5120, 2880)),
            Some(false)
        );
        // The display fits the box: native either way, nothing learned.
        assert_eq!(host_scales((1920, 1080), (2008, 1232), (1920, 1080)), None);
        // Planted negatives: no picture yet; a sub-64 box keeps native by rule.
        assert_eq!(host_scales((1920, 1080), (2008, 1232), (0, 0)), None);
        assert_eq!(host_scales((1920, 1080), (40, 1000), (1920, 1080)), None);
    }

    #[test]
    fn a_resize_reopens_only_when_the_host_would_send_a_different_picture() {
        let display = (1920, 1080);
        // The whole display already fits and still does (a maximize on a
        // larger screen): native pixels either way, no reopen.
        assert!(!reopen_for(display, (1920, 1080), (2256, 1440)));
        assert!(!reopen_for(display, (1920, 1080), (1920, 1080)));
        // Shrunk below the display: the host must fit a smaller picture.
        assert!(reopen_for(display, (1920, 1080), (1280, 1000)));
        // Grown but still smaller than the display (a scaled 5K host).
        let display = (5120, 2880);
        assert!(reopen_for(display, (2008, 1129), (2256, 1440)));
        // The host's even-size rounding of the same box is not a change.
        assert!(!reopen_for(display, (2006, 1128), (2008, 1232)));
        // Planted negatives: no picture yet, unknown display, tiny window.
        assert!(!reopen_for(display, (0, 0), (1000, 800)));
        assert!(!reopen_for((0, 0), (2008, 1129), (1000, 800)));
        assert!(!reopen_for(display, (2008, 1129), (63, 800)));
    }

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

#[cfg(test)]
#[path = "geometry/scaled_tests.rs"]
mod scaled_tests;

#[cfg(test)]
#[path = "geometry/native_tests.rs"]
mod native_tests;
