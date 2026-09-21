//! Distinct coordinate systems and checked conversions for Linux hosting (plan §10.1).
//!
//! Kept strictly distinct:
//! 1. Compositor-space coordinates (Wayland surface / layout units).
//! 2. Stream pixels (`PipeWire` capture buffer raster).
//! 3. Crop/scale mapping (sub-rectangle and scaling parameters).
//! 4. EIS region mapping (libei device regions and scaling).
//!
//! All conversions use checked arithmetic and reject non-finite, out-of-bounds,
//! or division-by-zero inputs.

use core::fmt;

/// Point in compositor desktop layout space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositorPoint {
    pub x: f64,
    pub y: f64,
}

impl CompositorPoint {
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// Rectangle in compositor layout space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositorRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl CompositorRect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Result<Self, CoordinateConversionError> {
        if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite() {
            return Err(CoordinateConversionError::NonFiniteCoordinate);
        }
        if width <= 0.0 || height <= 0.0 {
            return Err(CoordinateConversionError::InvalidDimensions);
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    #[must_use]
    pub fn contains(self, pt: CompositorPoint) -> bool {
        pt.is_finite()
            && pt.x >= self.x
            && pt.x <= self.x + self.width
            && pt.y >= self.y
            && pt.y <= self.y + self.height
    }
}

/// Raster pixel coordinates within the `PipeWire` stream buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamPixelPoint {
    pub x: u32,
    pub y: u32,
}

/// Dimensions of the `PipeWire` stream buffer in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamResolution {
    pub width: u32,
    pub height: u32,
}

impl StreamResolution {
    pub const fn new(width: u32, height: u32) -> Result<Self, CoordinateConversionError> {
        if width == 0 || height == 0 {
            return Err(CoordinateConversionError::InvalidDimensions);
        }
        Ok(Self { width, height })
    }
}

/// Crop region and scale factor applied between compositor space and captured stream pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropScaleMapping {
    pub crop_offset_x: f64,
    pub crop_offset_y: f64,
    pub crop_width: f64,
    pub crop_height: f64,
    pub scale_factor: f64,
}

impl CropScaleMapping {
    pub fn new(
        crop_offset_x: f64,
        crop_offset_y: f64,
        crop_width: f64,
        crop_height: f64,
        scale_factor: f64,
    ) -> Result<Self, CoordinateConversionError> {
        if !crop_offset_x.is_finite()
            || !crop_offset_y.is_finite()
            || !crop_width.is_finite()
            || !crop_height.is_finite()
            || !scale_factor.is_finite()
        {
            return Err(CoordinateConversionError::NonFiniteCoordinate);
        }
        if crop_width <= 0.0 || crop_height <= 0.0 {
            return Err(CoordinateConversionError::InvalidDimensions);
        }
        if scale_factor <= 0.0 {
            return Err(CoordinateConversionError::InvalidScaleFactor);
        }
        Ok(Self {
            crop_offset_x,
            crop_offset_y,
            crop_width,
            crop_height,
            scale_factor,
        })
    }

    /// Full 1:1 uncropped mapping for a given resolution.
    pub fn identity(resolution: StreamResolution) -> Self {
        Self {
            crop_offset_x: 0.0,
            crop_offset_y: 0.0,
            crop_width: f64::from(resolution.width),
            crop_height: f64::from(resolution.height),
            scale_factor: 1.0,
        }
    }
}

/// EIS device region reported by libei (contains region offset, physical dimensions, and scale).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EisRegion {
    pub offset_x: i32,
    pub offset_y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

impl EisRegion {
    pub fn new(
        offset_x: i32,
        offset_y: i32,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Result<Self, CoordinateConversionError> {
        if width == 0 || height == 0 {
            return Err(CoordinateConversionError::InvalidDimensions);
        }
        if !scale.is_finite() || scale <= 0.0 {
            return Err(CoordinateConversionError::InvalidScaleFactor);
        }
        Ok(Self {
            offset_x,
            offset_y,
            width,
            height,
            scale,
        })
    }
}

/// Point in EIS device coordinate space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EisPoint {
    pub x: f64,
    pub y: f64,
}

impl EisPoint {
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// Typed errors in coordinate conversions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CoordinateConversionError {
    NonFiniteCoordinate,
    InvalidDimensions,
    InvalidScaleFactor,
    OutOfBounds {
        val_x: f64,
        val_y: f64,
        max_x: f64,
        max_y: f64,
    },
    Overflow,
}

impl fmt::Display for CoordinateConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteCoordinate => write!(f, "coordinate is NaN or infinite"),
            Self::InvalidDimensions => write!(f, "width or height must be strictly positive"),
            Self::InvalidScaleFactor => {
                write!(f, "scale factor must be strictly positive and finite")
            }
            Self::OutOfBounds {
                val_x,
                val_y,
                max_x,
                max_y,
            } => write!(
                f,
                "coordinate ({val_x}, {val_y}) is out of bounds [0, {max_x}] x [0, {max_y}]"
            ),
            Self::Overflow => write!(f, "coordinate calculation resulted in arithmetic overflow"),
        }
    }
}

impl std::error::Error for CoordinateConversionError {}

/// Convert a compositor point into stream pixel coordinates using the crop/scale mapping.
pub fn compositor_to_stream_pixel(
    pt: CompositorPoint,
    mapping: &CropScaleMapping,
    resolution: StreamResolution,
) -> Result<StreamPixelPoint, CoordinateConversionError> {
    if !pt.is_finite() {
        return Err(CoordinateConversionError::NonFiniteCoordinate);
    }

    let rel_x = pt.x - mapping.crop_offset_x;
    let rel_y = pt.y - mapping.crop_offset_y;

    if rel_x < 0.0 || rel_y < 0.0 || rel_x > mapping.crop_width || rel_y > mapping.crop_height {
        return Err(CoordinateConversionError::OutOfBounds {
            val_x: rel_x,
            val_y: rel_y,
            max_x: mapping.crop_width,
            max_y: mapping.crop_height,
        });
    }

    let float_x = (rel_x * mapping.scale_factor).round();
    let float_y = (rel_y * mapping.scale_factor).round();

    if float_x < 0.0
        || float_y < 0.0
        || float_x > f64::from(u32::MAX)
        || float_y > f64::from(u32::MAX)
    {
        return Err(CoordinateConversionError::Overflow);
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let px_x = float_x as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let px_y = float_y as u32;

    if px_x > resolution.width || px_y > resolution.height {
        return Err(CoordinateConversionError::OutOfBounds {
            val_x: f64::from(px_x),
            val_y: f64::from(px_y),
            max_x: f64::from(resolution.width),
            max_y: f64::from(resolution.height),
        });
    }

    Ok(StreamPixelPoint { x: px_x, y: px_y })
}

/// Convert stream pixel coordinates back to compositor layout space.
pub fn stream_pixel_to_compositor(
    px: StreamPixelPoint,
    mapping: &CropScaleMapping,
) -> Result<CompositorPoint, CoordinateConversionError> {
    if mapping.scale_factor <= 0.0 || !mapping.scale_factor.is_finite() {
        return Err(CoordinateConversionError::InvalidScaleFactor);
    }

    let rel_x = f64::from(px.x) / mapping.scale_factor;
    let rel_y = f64::from(px.y) / mapping.scale_factor;

    let comp_x = mapping.crop_offset_x + rel_x;
    let comp_y = mapping.crop_offset_y + rel_y;

    if !comp_x.is_finite() || !comp_y.is_finite() {
        return Err(CoordinateConversionError::Overflow);
    }

    Ok(CompositorPoint::new(comp_x, comp_y))
}

/// Convert a compositor point into EIS device coordinates for a specific region.
pub fn compositor_to_eis(
    pt: CompositorPoint,
    region: &EisRegion,
) -> Result<EisPoint, CoordinateConversionError> {
    if !pt.is_finite() {
        return Err(CoordinateConversionError::NonFiniteCoordinate);
    }

    let rel_x = pt.x - f64::from(region.offset_x);
    let rel_y = pt.y - f64::from(region.offset_y);

    let max_w = f64::from(region.width);
    let max_h = f64::from(region.height);

    if rel_x < 0.0 || rel_y < 0.0 || rel_x > max_w || rel_y > max_h {
        return Err(CoordinateConversionError::OutOfBounds {
            val_x: rel_x,
            val_y: rel_y,
            max_x: max_w,
            max_y: max_h,
        });
    }

    let eis_x = rel_x * region.scale;
    let eis_y = rel_y * region.scale;

    if !eis_x.is_finite() || !eis_y.is_finite() {
        return Err(CoordinateConversionError::Overflow);
    }

    Ok(EisPoint::new(eis_x, eis_y))
}

/// Convert an EIS point back to compositor layout space.
pub fn eis_to_compositor(
    pt: EisPoint,
    region: &EisRegion,
) -> Result<CompositorPoint, CoordinateConversionError> {
    if !pt.is_finite() {
        return Err(CoordinateConversionError::NonFiniteCoordinate);
    }
    if region.scale <= 0.0 || !region.scale.is_finite() {
        return Err(CoordinateConversionError::InvalidScaleFactor);
    }

    let unscaled_x = pt.x / region.scale;
    let unscaled_y = pt.y / region.scale;

    let comp_x = f64::from(region.offset_x) + unscaled_x;
    let comp_y = f64::from(region.offset_y) + unscaled_y;

    if !comp_x.is_finite() || !comp_y.is_finite() {
        return Err(CoordinateConversionError::Overflow);
    }

    Ok(CompositorPoint::new(comp_x, comp_y))
}
