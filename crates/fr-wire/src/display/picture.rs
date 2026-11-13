//! Scaled pictures. A viewer that selects [`CAPABILITY`] states the largest
//! picture it can show 1:1 (its screen's pixels) in `Selection::picture`, and
//! a host that can scale its capture encodes the largest picture of the
//! display that fits inside it instead of the display's native pixels. Pixels
//! the viewer would only throw away are then never captured, encoded, sent or
//! decoded. Without the capability every picture is the display's native size.
//!
//! Both ends compute the same picture from the same numbers. A picture is
//! either the display's native size or canonical: an even width below the
//! native width, with the height [`Display::is_picture`] derives from it. A
//! viewer accepts any canonical picture (a shared source keeps the picture
//! its first viewer asked for), and its own limits still bound what it decodes.
use super::Display;
use crate::{
    WireError,
    negotiation::{Capability, Error, MAX_CAPABILITIES, Offer},
};

pub const CAPABILITY: &str = "scaled-picture";
pub const VERSION: u16 = 1;
/// The smallest box either side states or accepts, per axis.
pub const MIN_BOX_PIXELS: u32 = 64;

impl Display {
    /// The picture a host encodes for a viewer that can show at most `within`
    /// (pixel width, height) 1:1. The native size when there is no box or the
    /// display already fits; otherwise the widest canonical picture inside the
    /// box. Never larger than the display.
    pub fn picture(&self, within: Option<(u32, u32)>) -> Result<(u32, u32), WireError> {
        let (width, height) = (self.pixel_width, self.pixel_height);
        if width == 0 || height == 0 {
            return Err(WireError::InvalidValue);
        }
        let Some((box_width, box_height)) = within else {
            return Ok((width, height));
        };
        if box_width < MIN_BOX_PIXELS || box_height < MIN_BOX_PIXELS {
            return Err(WireError::InvalidValue);
        }
        if width <= box_width && height <= box_height {
            return Ok((width, height));
        }
        // The widest width whose derived height still fits the box: that
        // height, `w * height / width` rounded down to even, is at most
        // `box_height` exactly when `w * height < (even(box_height) + 2) * width`.
        let limit = (u64::from(box_height & !1) + 2) * u64::from(width);
        let by_height = (limit - 1) / u64::from(height);
        let fitted = by_height
            .min(u64::from(box_width))
            .min(u64::from(width) - 1)
            & !1;
        let fitted = u32::try_from(fitted).map_err(|_| WireError::ArithmeticOverflow)?;
        let derived = scaled_height(width, height, fitted);
        if fitted < 2 || derived < 2 {
            return Err(WireError::ResourceLimit);
        }
        Ok((fitted, derived))
    }

    /// Whether `(width, height)` is this display's native size or one of its
    /// canonical scaled pictures.
    pub fn is_picture(&self, width: u32, height: u32) -> bool {
        is_picture((self.pixel_width, self.pixel_height), (width, height))
    }

    /// Whether a session may carry a picture of `width` x `height` from this
    /// display: its native size always, and a canonical scaled picture only
    /// when the session selected scaled pictures (`scaled`).
    pub fn accepts_picture(&self, scaled: bool, width: u32, height: u32) -> bool {
        if scaled {
            self.is_picture(width, height)
        } else {
            (width, height) == (self.pixel_width, self.pixel_height)
        }
    }
}

/// The canonical picture rule, also used by the private presentation worker
/// when binding decoded pixels to the original display's input coordinates.
/// Callers validate resource limits independently; dimensions alone carry no
/// session, display-selection or input authority.
pub fn is_picture(display: (u32, u32), picture: (u32, u32)) -> bool {
    let (native_width, native_height) = display;
    let (width, height) = picture;
    native_width != 0
        && native_height != 0
        && ((width, height) == (native_width, native_height)
            || (width >= 2
                && width < native_width
                && width.is_multiple_of(2)
                && height >= 2
                && height == scaled_height(native_width, native_height, width)))
}

/// The even height of a picture `width` pixels wide with the display's aspect
/// ratio, rounded down. `width` never exceeds `native_width`, so the result
/// never exceeds `native_height`.
fn scaled_height(native_width: u32, native_height: u32, width: u32) -> u32 {
    let height = u64::from(width) * u64::from(native_height) / u64::from(native_width.max(1));
    u32::try_from(height).unwrap_or(u32::MAX) & !1
}

/// Whether a capability list (an offer's or a selection's) carries this one.
pub fn selected(capabilities: &[Capability]) -> bool {
    capabilities
        .iter()
        .any(|cap| cap.name == CAPABILITY && cap.version == VERSION)
}

/// Add the optional capability to an offer, in name order. A host offers it
/// only when its capture can scale; a viewer only when it states a box. When
/// the offer is already full, optional quality diagnostics yield first; if it
/// is still full the offer is returned unchanged and pictures stay native.
pub fn with_capability(mut offer: Offer) -> Result<Offer, Error> {
    offer.validate()?;
    if let Some(cap) = offer.capabilities.iter().find(|cap| cap.name == CAPABILITY) {
        return if cap.version == VERSION && !cap.required {
            Ok(offer)
        } else {
            Err(Error::RequiredCapability)
        };
    }
    if offer.capabilities.len() >= MAX_CAPABILITIES {
        offer
            .capabilities
            .retain(|cap| cap.name != crate::quality_decision::CAPABILITY || cap.required);
    }
    if offer.capabilities.len() >= MAX_CAPABILITIES {
        return Ok(offer);
    }
    offer
        .capabilities
        .try_reserve_exact(1)
        .map_err(|_| Error::Allocation)?;
    offer.capabilities.push(Capability {
        name: CAPABILITY.into(),
        version: VERSION,
        required: false,
    });
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer.validate()?;
    Ok(offer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::DisplayGeometryGeneration;

    fn display(pixel_width: u32, pixel_height: u32) -> Display {
        Display {
            handle: 1,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: 0,
            y: 0,
            pixel_width,
            pixel_height,
            logical_width: pixel_width,
            logical_height: pixel_height,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        }
    }

    #[test]
    fn a_5k_display_fits_a_1080p_screen_exactly() {
        let studio = display(5120, 2880);
        assert_eq!(studio.picture(Some((1920, 1080))), Ok((1920, 1080)));
        assert!(studio.is_picture(1920, 1080));
        // The same display for a Retina laptop and a landscape phone.
        assert_eq!(studio.picture(Some((3024, 1964))), Ok((3024, 1700)));
        assert_eq!(studio.picture(Some((2868, 1320))), Ok((2350, 1320)));
        assert!(studio.is_picture(3024, 1700) && studio.is_picture(2350, 1320));
    }

    #[test]
    fn native_pixels_without_a_box_or_when_they_already_fit() {
        let studio = display(5120, 2880);
        assert_eq!(studio.picture(None), Ok((5120, 2880)));
        assert_eq!(studio.picture(Some((5120, 2880))), Ok((5120, 2880)));
        assert_eq!(studio.picture(Some((6016, 3384))), Ok((5120, 2880)));
        // An odd native size is still its own picture; it is never scaled up.
        let odd = display(1365, 767);
        assert_eq!(odd.picture(Some((1920, 1080))), Ok((1365, 767)));
        assert!(odd.is_picture(1365, 767));
    }

    #[test]
    fn every_fitted_picture_is_canonical_inside_its_box_and_keeps_the_aspect() {
        for (width, height) in [
            (5120, 2880),
            (6144, 3456),
            (2560, 1600),
            (1080, 1920),
            (3441, 1441),
        ] {
            let source = display(width, height);
            for box_width in (64..=4096).step_by(97) {
                for box_height in (64..=4096).step_by(89) {
                    let (w, h) = source.picture(Some((box_width, box_height))).unwrap();
                    assert!(
                        source.is_picture(w, h),
                        "{width}x{height} in {box_width}x{box_height}"
                    );
                    assert!(w <= width && h <= height);
                    if (w, h) != (width, height) {
                        assert!(w <= box_width && h <= box_height);
                        assert!(w.is_multiple_of(2) && h.is_multiple_of(2));
                        // Within one even step of the display's aspect ratio.
                        let skew = (u64::from(w) * u64::from(height))
                            .abs_diff(u64::from(h) * u64::from(width));
                        assert!(
                            skew <= 2 * u64::from(width),
                            "{w}x{h} from {width}x{height}"
                        );
                        // The next canonical width would not fit the box.
                        let wider = w + 2;
                        assert!(
                            wider >= width
                                || wider > box_width
                                || scaled_height(width, height, wider) > box_height
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn only_a_scaled_session_accepts_a_scaled_picture() {
        let studio = display(5120, 2880);
        assert!(studio.accepts_picture(false, 5120, 2880));
        assert!(studio.accepts_picture(true, 5120, 2880));
        assert!(studio.accepts_picture(true, 1920, 1080));
        assert!(!studio.accepts_picture(false, 1920, 1080));
        assert!(!studio.accepts_picture(true, 1920, 1082));
    }

    #[test]
    fn only_native_or_canonical_sizes_are_pictures() {
        let studio = display(5120, 2880);
        for (w, h) in [
            (1920, 1082),
            (1921, 1080),
            (1920, 1078),
            (5122, 2880),
            (0, 0),
            (5120, 2878),
        ] {
            assert!(!studio.is_picture(w, h), "{w}x{h}");
        }
        assert_eq!(
            studio.picture(Some((63, 1080))),
            Err(WireError::InvalidValue)
        );
        assert_eq!(
            studio.picture(Some((1920, 63))),
            Err(WireError::InvalidValue)
        );
        assert_eq!(display(0, 10).picture(None), Err(WireError::InvalidValue));
        // A sliver display can leave no canonical picture inside a box.
        assert_eq!(
            display(8000, 2).picture(Some((64, 64))),
            Err(WireError::ResourceLimit)
        );
    }
}
