//! Bounded Desktop Duplication pointer pixels and composition bookkeeping.
//! DXGI positions name the shape's top-left, not its hotspot. Its monochrome
//! height includes both masks. Keep this policy independent of D3D so the
//! exact clipping, blending and underlay invalidation can run on every host.
#![forbid(unsafe_code)]

/// The same 256-pixel edge and 256 KiB byte ceiling as the cursor wire lane.
/// Each native scratch texture is one BGRA image of at most this size.
pub(super) const MAX_SIDE: u32 = 256;
pub(super) const MAX_BYTES: usize = MAX_SIDE as usize * MAX_SIDE as usize * 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Monochrome,
    Color,
    MaskedColor,
}

/// Validated pixels from the same acquired DXGI frame as the position update.
pub(super) struct Shape {
    kind: Kind,
    width: u32,
    height: u32,
    pitch: usize,
    bytes: Vec<u8>,
}

impl Shape {
    pub(super) fn new(
        kind: Kind,
        width: u32,
        reported_height: u32,
        pitch: u32,
        mut bytes: Vec<u8>,
    ) -> Option<Self> {
        let height = if kind == Kind::Monochrome {
            if !reported_height.is_multiple_of(2) {
                return None;
            }
            reported_height / 2
        } else {
            reported_height
        };
        if !(1..=MAX_SIDE).contains(&width) || !(1..=MAX_SIDE).contains(&height) {
            return None;
        }
        let row_bytes = if kind == Kind::Monochrome {
            width.div_ceil(8)
        } else {
            width.checked_mul(4)?
        };
        let required = usize::try_from(pitch.checked_mul(reported_height)?).ok()?;
        if pitch < row_bytes || required > bytes.len() || bytes.len() > MAX_BYTES {
            return None;
        }
        bytes.truncate(required);
        let pitch = usize::try_from(pitch).ok()?;
        if kind == Kind::MaskedColor
            && bytes.chunks_exact(pitch).any(|row| {
                row[..row_bytes as usize]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| !matches!(pixel[3], 0 | 255))
            })
        {
            return None;
        }
        Some(Self {
            kind,
            width,
            height,
            pitch,
            bytes,
        })
    }

    /// The clipped cursor rectangle, with its offset inside the shape.
    pub(super) fn region(&self, position: (i32, i32), size: (u32, u32)) -> Option<Region> {
        let (x, y) = (i64::from(position.0), i64::from(position.1));
        let left = x.max(0);
        let top = y.max(0);
        let right = (x + i64::from(self.width)).min(i64::from(size.0));
        let bottom = (y + i64::from(self.height)).min(i64::from(size.1));
        if right <= left || bottom <= top {
            return None;
        }
        Some(Region {
            x: u32::try_from(left).ok()?,
            y: u32::try_from(top).ok()?,
            width: u32::try_from(right - left).ok()?,
            height: u32::try_from(bottom - top).ok()?,
            source_x: u32::try_from(left - x).ok()?,
            source_y: u32::try_from(top - y).ok()?,
        })
    }

    /// Blend one tightly bounded BGRA desktop row in place. No pixel outside
    /// the clipped cursor region, including D3D row padding, is read/written.
    pub(super) fn blend_row(&self, x: u32, y: u32, desktop: &mut [u8]) -> Option<()> {
        let pixels = u32::try_from(desktop.len() / 4).ok()?;
        if !desktop.len().is_multiple_of(4)
            || x.checked_add(pixels)? > self.width
            || y >= self.height
        {
            return None;
        }
        let row = y as usize * self.pitch;
        for (column, pixel) in desktop.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let column = x as usize + column;
            match self.kind {
                Kind::Monochrome => {
                    let bit = 0x80 >> (column % 8);
                    let and = self.bytes[row + column / 8] & bit != 0;
                    let xor =
                        self.bytes[row + self.height as usize * self.pitch + column / 8] & bit != 0;
                    for value in &mut pixel[..3] {
                        *value = (if and { *value } else { 0 }) ^ if xor { 255 } else { 0 };
                    }
                }
                Kind::Color | Kind::MaskedColor => {
                    let source = &self.bytes[row + column * 4..][..4];
                    for (value, &foreground) in pixel[..3].iter_mut().zip(&source[..3]) {
                        *value = match self.kind {
                            Kind::MaskedColor if source[3] == 255 => *value ^ foreground,
                            Kind::MaskedColor => foreground,
                            // DXGI's color pointer uses straight alpha, as in
                            // Microsoft's Desktop Duplication sample's blend.
                            _ => {
                                let alpha = u32::from(source[3]);
                                let mixed = u32::from(foreground) * alpha
                                    + u32::from(*value) * (255 - alpha);
                                u8::try_from((mixed + 127) / 255).ok()?
                            }
                        };
                    }
                }
            }
            // The selected desktop is opaque, including invert/mask results.
            pixel[3] = 255;
        }
        Some(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub source_x: u32,
    pub source_y: u32,
}

/// No position means that DXGI reports no separate pointer: it is hidden,
/// outside this output, or already part of the desktop pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Pointer {
    revision: u64,
    position: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Position {
    Unchanged,
    Hidden,
    Visible((i32, i32)),
}

#[derive(Default)]
pub(super) struct State {
    pointer: Pointer,
    shape: Option<Shape>,
    /// Clean pixels under the last overlay, valid only for this desktop image.
    underlay: Option<Region>,
}

impl State {
    pub(super) const fn pointer(&self) -> Pointer {
        self.pointer
    }

    pub(super) const fn has_shape(&self) -> bool {
        self.shape.is_some()
    }

    /// Only after `ReleaseFrame` succeeds. `Position::Unchanged` means this
    /// acquisition had `LastMouseUpdateTime == 0`, so its position fields are
    /// unspecified; retain the last successful update from this duplication.
    /// `Position::Hidden` is an explicit invisible/already-composited update.
    pub(super) fn released(
        &mut self,
        position: Position,
        shape: Option<Shape>,
        new_image: bool,
    ) -> Option<()> {
        let position = match position {
            Position::Unchanged => self.pointer.position,
            Position::Hidden => None,
            Position::Visible(position) => Some(position),
        };
        if position.is_some() && shape.is_none() && self.shape.is_none() {
            return None;
        }
        if let Some(shape) = shape {
            self.pointer.revision = self.pointer.revision.checked_add(1)?;
            self.shape = Some(shape);
        }
        self.pointer.position = position;
        if new_image {
            // A full image copy has already removed the old overlay. Its
            // saved background must not be restored onto this newer image.
            self.underlay = None;
        }
        Some(())
    }

    pub(super) fn visible(&self, size: (u32, u32)) -> Option<(&Shape, Region)> {
        let shape = self.shape.as_ref()?;
        let region = shape.region(self.pointer.position?, size)?;
        Some((shape, region))
    }

    /// Losing/replacing the duplication invalidates shape and position too.
    pub(super) fn invalidate(&mut self) {
        *self = Self::default();
    }

    pub(super) fn take_underlay(&mut self) -> Option<Region> {
        self.underlay.take()
    }

    pub(super) fn drawn(&mut self, region: Region) {
        self.underlay = Some(region);
    }
}
