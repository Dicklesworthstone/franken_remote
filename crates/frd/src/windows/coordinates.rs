//! Multi-monitor virtual desktop coordinates and rotation mappings for Windows (plan §10.3).
//!
//! Enforces:
//! 1. Checked coordinate translation across multi-monitor setups with negative virtual desktop coordinates.
//! 2. DXGI display rotation (`DXGI_MODE_ROTATION`) mapping (`Identity`, `Rotate90`, `Rotate180`, `Rotate270`).
//! 3. Normalized input mapping for `SendInput` (`0..65535` across virtual desktop bounds).
//! 4. Per-monitor DPI scaling transformations without floating-point overflow or silent truncation.

use std::fmt;

/// Errors occurring during coordinate translation across Windows virtual desktop systems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinateError {
    /// Dimension or coordinate computation overflowed.
    ArithmeticOverflow,
    /// Invalid bounds (e.g. width or height is zero or negative).
    InvalidBounds { width: i32, height: i32 },
    /// Input point falls outside the designated display output rectangle.
    OutOfBounds { x: i32, y: i32 },
    /// Division by zero in scaling or normalization calculation.
    ZeroDimension,
}

impl fmt::Display for CoordinateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArithmeticOverflow => write!(f, "coordinate arithmetic overflow"),
            Self::InvalidBounds { width, height } => {
                write!(f, "invalid display bounds: {width}x{height}")
            }
            Self::OutOfBounds { x, y } => {
                write!(f, "coordinate ({x}, {y}) out of output bounds")
            }
            Self::ZeroDimension => write!(f, "dimension is zero during normalization"),
        }
    }
}

impl std::error::Error for CoordinateError {}

/// DXGI display rotation corresponding to `DXGI_MODE_ROTATION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DxgiRotation {
    /// No rotation (0 degrees).
    #[default]
    Identity = 1,
    /// 90 degrees clockwise.
    Rotate90 = 2,
    /// 180 degrees.
    Rotate180 = 3,
    /// 270 degrees clockwise.
    Rotate270 = 4,
}

/// Point in Windows virtual desktop coordinates (may have negative coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtualDesktopPoint {
    pub x: i32,
    pub y: i32,
}

/// Bounding rectangle for a display or the entire virtual desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDesktopRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl VirtualDesktopRect {
    /// Create a new virtual desktop rectangle.
    #[must_use]
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Width of the rectangle. Returns an error if right < left or overflow occurs.
    pub fn width(&self) -> Result<u32, CoordinateError> {
        let diff = self
            .right
            .checked_sub(self.left)
            .ok_or(CoordinateError::ArithmeticOverflow)?;
        if diff <= 0 {
            return Err(CoordinateError::InvalidBounds {
                width: diff,
                height: 0,
            });
        }
        #[allow(clippy::cast_sign_loss)]
        Ok(diff as u32)
    }

    /// Height of the rectangle. Returns an error if bottom < top or overflow occurs.
    pub fn height(&self) -> Result<u32, CoordinateError> {
        let diff = self
            .bottom
            .checked_sub(self.top)
            .ok_or(CoordinateError::ArithmeticOverflow)?;
        if diff <= 0 {
            return Err(CoordinateError::InvalidBounds {
                width: 0,
                height: diff,
            });
        }
        #[allow(clippy::cast_sign_loss)]
        Ok(diff as u32)
    }

    /// Verify if a point lies within this rectangle [left, right) x [top, bottom).
    #[must_use]
    pub const fn contains(&self, point: VirtualDesktopPoint) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }
}

/// Physical pixel resolution of a captured stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamResolution {
    pub width: u32,
    pub height: u32,
}

/// Pixel point within a captured stream [0, width) x [0, height).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPixelPoint {
    pub x: u32,
    pub y: u32,
}

/// Normalized coordinate for `SendInput` with `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.
/// Both coordinates span [0, 65535] across the entire virtual screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizedSendInputPoint {
    pub x: u16,
    pub y: u16,
}

/// Map a pixel on a captured display output back to Windows virtual desktop coordinates,
/// respecting display orientation rotation.
pub fn stream_pixel_to_virtual_desktop(
    pixel: StreamPixelPoint,
    stream_res: StreamResolution,
    output_rect: VirtualDesktopRect,
    rotation: DxgiRotation,
) -> Result<VirtualDesktopPoint, CoordinateError> {
    if stream_res.width == 0 || stream_res.height == 0 {
        return Err(CoordinateError::ZeroDimension);
    }
    if pixel.x >= stream_res.width || pixel.y >= stream_res.height {
        return Err(CoordinateError::OutOfBounds {
            x: pixel.x.cast_signed(),
            y: pixel.y.cast_signed(),
        });
    }

    let out_w = output_rect.width()?;
    let out_h = output_rect.height()?;

    // Map according to rotation
    let (unrotated_x, unrotated_y) = match rotation {
        DxgiRotation::Identity => (pixel.x, pixel.y),
        DxgiRotation::Rotate90 => {
            // Rotated 90 deg clockwise: captured width is out_h, captured height is out_w
            let mapped_x = stream_res
                .width
                .checked_sub(1)
                .and_then(|m| m.checked_sub(pixel.x))
                .ok_or(CoordinateError::ArithmeticOverflow)?;
            (pixel.y, mapped_x)
        }
        DxgiRotation::Rotate180 => {
            let mapped_x = stream_res
                .width
                .checked_sub(1)
                .and_then(|m| m.checked_sub(pixel.x))
                .ok_or(CoordinateError::ArithmeticOverflow)?;
            let mapped_y = stream_res
                .height
                .checked_sub(1)
                .and_then(|m| m.checked_sub(pixel.y))
                .ok_or(CoordinateError::ArithmeticOverflow)?;
            (mapped_x, mapped_y)
        }
        DxgiRotation::Rotate270 => {
            let mapped_y = stream_res
                .height
                .checked_sub(1)
                .and_then(|m| m.checked_sub(pixel.y))
                .ok_or(CoordinateError::ArithmeticOverflow)?;
            (mapped_y, pixel.x)
        }
    };

    // Scale from unrotated pixel coordinates to display output rect dimensions
    let vx = if stream_res.width == out_w {
        output_rect
            .left
            .checked_add_unsigned(unrotated_x)
            .ok_or(CoordinateError::ArithmeticOverflow)?
    } else {
        let scaled = u64::from(unrotated_x)
            .checked_mul(u64::from(out_w))
            .and_then(|p| p.checked_div(u64::from(stream_res.width)))
            .ok_or(CoordinateError::ArithmeticOverflow)?;
        let scaled_u32 = u32::try_from(scaled).map_err(|_| CoordinateError::ArithmeticOverflow)?;
        output_rect
            .left
            .checked_add_unsigned(scaled_u32)
            .ok_or(CoordinateError::ArithmeticOverflow)?
    };

    let vy = if stream_res.height == out_h {
        output_rect
            .top
            .checked_add_unsigned(unrotated_y)
            .ok_or(CoordinateError::ArithmeticOverflow)?
    } else {
        let scaled = u64::from(unrotated_y)
            .checked_mul(u64::from(out_h))
            .and_then(|p| p.checked_div(u64::from(stream_res.height)))
            .ok_or(CoordinateError::ArithmeticOverflow)?;
        let scaled_u32 = u32::try_from(scaled).map_err(|_| CoordinateError::ArithmeticOverflow)?;
        output_rect
            .top
            .checked_add_unsigned(scaled_u32)
            .ok_or(CoordinateError::ArithmeticOverflow)?
    };

    Ok(VirtualDesktopPoint { x: vx, y: vy })
}

/// Convert a point in Windows virtual desktop space into normalized [0, 65535]
/// coordinates suitable for `SendInput` with `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.
pub fn virtual_desktop_to_send_input(
    point: VirtualDesktopPoint,
    virtual_screen: VirtualDesktopRect,
) -> Result<NormalizedSendInputPoint, CoordinateError> {
    let virt_w = virtual_screen.width()?;
    let virt_h = virtual_screen.height()?;

    if !virtual_screen.contains(point) {
        return Err(CoordinateError::OutOfBounds {
            x: point.x,
            y: point.y,
        });
    }

    // Offset relative to virtual screen origin (which may be negative)
    let rel_x = point
        .x
        .checked_sub(virtual_screen.left)
        .ok_or(CoordinateError::ArithmeticOverflow)?;
    let rel_y = point
        .y
        .checked_sub(virtual_screen.top)
        .ok_or(CoordinateError::ArithmeticOverflow)?;

    if rel_x < 0 || rel_y < 0 {
        return Err(CoordinateError::OutOfBounds {
            x: point.x,
            y: point.y,
        });
    }

    #[allow(clippy::cast_sign_loss)]
    let offset_x = rel_x as u32;
    #[allow(clippy::cast_sign_loss)]
    let offset_y = rel_y as u32;

    // SendInput formula: (x - left) * 65535 / (width - 1)
    let norm_x = if virt_w <= 1 {
        0
    } else {
        let dividend = u64::from(offset_x).checked_mul(65535).unwrap_or(0);
        let divisor = u64::from(virt_w.saturating_sub(1));
        let q = dividend
            .checked_div(divisor)
            .ok_or(CoordinateError::ArithmeticOverflow)?;
        u16::try_from(q.min(65535)).unwrap_or(65535)
    };

    let norm_y = if virt_h <= 1 {
        0
    } else {
        let dividend = u64::from(offset_y).checked_mul(65535).unwrap_or(0);
        let divisor = u64::from(virt_h.saturating_sub(1));
        let q = dividend
            .checked_div(divisor)
            .ok_or(CoordinateError::ArithmeticOverflow)?;
        u16::try_from(q.min(65535)).unwrap_or(65535)
    };

    Ok(NormalizedSendInputPoint {
        x: norm_x,
        y: norm_y,
    })
}
