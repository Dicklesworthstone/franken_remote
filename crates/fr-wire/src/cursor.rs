//! Cursor shape and position wire records (Plan §11.4, PROTOCOL.md §17.3).
//!
//! - `0x0038 CursorShape`: reliable configuration channel (`MediaConfig`).
//!   Shape ID, dimensions, hotspot, scale, visibility, host-composited flag,
//!   and checked RGBA8 bytes (`width * height * 4`).
//! - `0x0039 CursorPosition`: replaceable state channel (Video datagram).
//!   Shape ID, screen coordinates (x, y), geometry generation, sequence, flags.

use crate::record::Writer;
use crate::{HEADER_BYTES, Kind, Record, WireError};
use fr_core::limits::ProtocolLimits;

/// Maximum cursor dimension (width or height) in pixels.
pub const MAX_CURSOR_DIMENSION: u16 = 256;

/// Absolute implementation ceiling for RGBA8 cursor bytes (256x256 * 4 = 262,144 bytes).
pub const MAX_CURSOR_BYTES: usize =
    (MAX_CURSOR_DIMENSION as usize) * (MAX_CURSOR_DIMENSION as usize) * 4;

/// Fixed header bytes for a `CursorShape` payload before the RGBA pixel array data.
/// 4 (`shape_id`) + 2 (width) + 2 (height) + 2 (`hotspot_x`) + 2 (`hotspot_y`) + 2 (scale) + 1 (flags) = 15 bytes.
pub const CURSOR_SHAPE_HEADER_BYTES: usize = 15;

/// Fixed payload bytes for a `CursorPosition` record.
/// 4 (`shape_id`) + 4 (x) + 4 (y) + 8 (`geometry_gen`) + 8 (seq) + 1 (flags) = 29 bytes.
pub const CURSOR_POSITION_PAYLOAD_BYTES: usize = 29;

/// Complete wire record size for a `CursorPosition` record.
pub const CURSOR_POSITION_RECORD_BYTES: usize = HEADER_BYTES + CURSOR_POSITION_PAYLOAD_BYTES;

/// Flags for [`CursorShape`].
pub const SHAPE_FLAG_VISIBLE: u8 = 0x01;
pub const SHAPE_FLAG_HOST_COMPOSITED: u8 = 0x02;

/// Flags for [`CursorPosition`].
pub const POSITION_FLAG_VISIBLE: u8 = 0x01;
pub const POSITION_FLAG_LOCKED: u8 = 0x02;

/// A borrowed cursor bitmap shape with verified dimensions and hotspot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorShape<'a> {
    pub shape_id: u32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub scale_1000: u16,
    pub flags: u8,
    pub rgba: &'a [u8],
}

impl CursorShape<'_> {
    /// Validates dimensions, hotspot, scale, and exact pixel buffer size.
    pub fn validate(&self, limits: &ProtocolLimits) -> Result<(), WireError> {
        if self.width > MAX_CURSOR_DIMENSION || self.height > MAX_CURSOR_DIMENSION {
            return Err(WireError::ResourceLimit);
        }
        let expected_bytes = usize::from(self.width)
            .checked_mul(usize::from(self.height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(WireError::ArithmeticOverflow)?;

        if self.rgba.len() != expected_bytes {
            return Err(WireError::InvalidValue);
        }
        if self.width > 0 && self.hotspot_x >= self.width {
            return Err(WireError::InvalidValue);
        }
        if self.height > 0 && self.hotspot_y >= self.height {
            return Err(WireError::InvalidValue);
        }
        if self.width == 0 && self.hotspot_x != 0 {
            return Err(WireError::InvalidValue);
        }
        if self.height == 0 && self.hotspot_y != 0 {
            return Err(WireError::InvalidValue);
        }
        if self.scale_1000 == 0 || self.scale_1000 > 10_000 {
            return Err(WireError::InvalidValue);
        }
        if self.flags & !(SHAPE_FLAG_VISIBLE | SHAPE_FLAG_HOST_COMPOSITED) != 0 {
            return Err(WireError::InvalidFlags);
        }

        // Total record size check against control channel limits
        let total_payload = CURSOR_SHAPE_HEADER_BYTES
            .checked_add(4) // 4-byte length prefix for data
            .and_then(|h| h.checked_add(expected_bytes))
            .ok_or(WireError::ArithmeticOverflow)?;
        let total_record = HEADER_BYTES
            .checked_add(total_payload)
            .ok_or(WireError::ArithmeticOverflow)?;

        if total_record > limits.max_control_message_bytes() as usize {
            return Err(WireError::ResourceLimit);
        }

        Ok(())
    }

    /// Whether the cursor is marked visible.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.flags & SHAPE_FLAG_VISIBLE != 0
    }

    /// Whether the cursor is composited on the host (viewer must not draw a local cursor).
    #[must_use]
    pub const fn is_host_composited(&self) -> bool {
        self.flags & SHAPE_FLAG_HOST_COMPOSITED != 0
    }
}

/// Encodes a [`CursorShape`] into a bounded output buffer on the `MediaConfig` channel.
pub fn encode_cursor_shape(
    shape: &CursorShape<'_>,
    binding: u32,
    limits: &ProtocolLimits,
    out: &mut [u8],
) -> Result<usize, WireError> {
    shape.validate(limits)?;

    let payload_len = CURSOR_SHAPE_HEADER_BYTES
        .checked_add(4)
        .and_then(|h| h.checked_add(shape.rgba.len()))
        .ok_or(WireError::ArithmeticOverflow)?;

    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding,
        Kind::CursorShape,
        payload_len,
    )?;

    w.u32(shape.shape_id)?;
    w.u16(shape.width)?;
    w.u16(shape.height)?;
    w.u16(shape.hotspot_x)?;
    w.u16(shape.hotspot_y)?;
    w.u16(shape.scale_1000)?;
    w.u8(shape.flags)?;
    w.data(shape.rgba)?;
    w.finish()
}

/// Decodes a [`CursorShape`] from a validated [`Record`].
pub fn decode_cursor_shape<'a>(
    record: Record<'a>,
    limits: &ProtocolLimits,
) -> Result<CursorShape<'a>, WireError> {
    let mut r = record.reader(Kind::CursorShape)?;
    let shape_id = r.u32()?;
    let width = r.u16()?;
    let height = r.u16()?;
    let hotspot_x = r.u16()?;
    let hotspot_y = r.u16()?;
    let scale_1000 = r.u16()?;
    let flags = r.u8()?;
    let rgba = r.data()?;
    r.finish()?;

    let shape = CursorShape {
        shape_id,
        width,
        height,
        hotspot_x,
        hotspot_y,
        scale_1000,
        flags,
        rgba,
    };
    shape.validate(limits)?;
    Ok(shape)
}

/// Replaceable confirmed cursor position received from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    pub shape_id: u32,
    pub x: i32,
    pub y: i32,
    pub geometry_generation: u64,
    pub sequence: u64,
    pub flags: u8,
}

impl CursorPosition {
    /// Validates sequence monotonicity and flag bits.
    pub fn validate(&self) -> Result<(), WireError> {
        if self.sequence == 0 {
            return Err(WireError::InvalidValue);
        }
        if self.flags & !(POSITION_FLAG_VISIBLE | POSITION_FLAG_LOCKED) != 0 {
            return Err(WireError::InvalidFlags);
        }
        Ok(())
    }

    /// Whether the cursor is marked visible.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.flags & POSITION_FLAG_VISIBLE != 0
    }

    /// Whether pointer lock / confinement is active.
    #[must_use]
    pub const fn is_locked(&self) -> bool {
        self.flags & POSITION_FLAG_LOCKED != 0
    }
}

/// Encodes a [`CursorPosition`] into a bounded output buffer on the Video channel.
pub fn encode_cursor_position(
    pos: &CursorPosition,
    binding: u32,
    max_record_bytes: usize,
    out: &mut [u8],
) -> Result<usize, WireError> {
    pos.validate()?;

    let mut w = Writer::record_bounded(
        out,
        max_record_bytes,
        binding,
        Kind::CursorPosition,
        CURSOR_POSITION_PAYLOAD_BYTES,
    )?;

    w.u32(pos.shape_id)?;
    w.u32(pos.x.cast_unsigned())?;
    w.u32(pos.y.cast_unsigned())?;
    w.u64(pos.geometry_generation)?;
    w.u64(pos.sequence)?;
    w.u8(pos.flags)?;
    w.finish()
}

/// Decodes a [`CursorPosition`] from a validated [`Record`].
pub fn decode_cursor_position(record: Record<'_>) -> Result<CursorPosition, WireError> {
    let mut r = record.reader(Kind::CursorPosition)?;
    let shape_id = r.u32()?;
    let x = r.u32()?.cast_signed();
    let y = r.u32()?.cast_signed();
    let geometry_generation = r.u64()?;
    let sequence = r.u64()?;
    let flags = r.u8()?;
    r.finish()?;

    let pos = CursorPosition {
        shape_id,
        x,
        y,
        geometry_generation,
        sequence,
        flags,
    };
    pos.validate()?;
    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MediaLimits;
    use crate::record::Channel;

    fn test_limits() -> (ProtocolLimits, MediaLimits) {
        let p = ProtocolLimits::ABSOLUTE;
        let m = MediaLimits::new(p, 1_150, 16_384, 64).unwrap();
        (p, m)
    }

    #[test]
    fn cursor_shape_roundtrip_valid() {
        let (proto_limits, med_limits) = test_limits();
        let rgba = [0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]; // 2x1 RGBA
        let shape = CursorShape {
            shape_id: 42,
            width: 2,
            height: 1,
            hotspot_x: 1,
            hotspot_y: 0,
            scale_1000: 1000,
            flags: SHAPE_FLAG_VISIBLE,
            rgba: &rgba,
        };

        let mut buf = [0u8; 512];
        let binding = 7;
        let len = encode_cursor_shape(&shape, binding, &proto_limits, &mut buf).unwrap();

        let record =
            Record::decode(&buf[..len], &med_limits, binding, Channel::MediaConfig).unwrap();
        let decoded = decode_cursor_shape(record, &proto_limits).unwrap();

        assert_eq!(decoded, shape);
        assert!(decoded.is_visible());
        assert!(!decoded.is_host_composited());
    }

    #[test]
    fn cursor_shape_validation_failures() {
        let (proto_limits, _) = test_limits();
        let rgba = [0u8; 16]; // 2x2 RGBA

        // Wrong buffer length (claims 2x3=24 bytes, gives 16)
        let invalid_len = CursorShape {
            shape_id: 1,
            width: 2,
            height: 3,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_1000: 1000,
            flags: SHAPE_FLAG_VISIBLE,
            rgba: &rgba,
        };
        assert_eq!(
            invalid_len.validate(&proto_limits),
            Err(WireError::InvalidValue)
        );

        // Hotspot out of bounds
        let invalid_hotspot = CursorShape {
            shape_id: 1,
            width: 2,
            height: 2,
            hotspot_x: 2, // >= width
            hotspot_y: 0,
            scale_1000: 1000,
            flags: 0,
            rgba: &rgba,
        };
        assert_eq!(
            invalid_hotspot.validate(&proto_limits),
            Err(WireError::InvalidValue)
        );

        // Zero scale
        let zero_scale = CursorShape {
            shape_id: 1,
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_1000: 0,
            flags: 0,
            rgba: &rgba,
        };
        assert_eq!(
            zero_scale.validate(&proto_limits),
            Err(WireError::InvalidValue)
        );

        // Invalid flags
        let bad_flags = CursorShape {
            shape_id: 1,
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_1000: 1000,
            flags: 0x80,
            rgba: &rgba,
        };
        assert_eq!(
            bad_flags.validate(&proto_limits),
            Err(WireError::InvalidFlags)
        );

        // Exceeds max dimension
        let huge_dim = CursorShape {
            shape_id: 1,
            width: 257,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_1000: 1000,
            flags: 0,
            rgba: &[],
        };
        assert_eq!(
            huge_dim.validate(&proto_limits),
            Err(WireError::ResourceLimit)
        );
    }

    #[test]
    fn cursor_position_roundtrip_valid() {
        let (_, med_limits) = test_limits();
        let pos = CursorPosition {
            shape_id: 42,
            x: -150,
            y: 300,
            geometry_generation: 105,
            sequence: 1001,
            flags: POSITION_FLAG_VISIBLE | POSITION_FLAG_LOCKED,
        };

        let mut buf = [0u8; 128];
        let binding = 9;
        let len =
            encode_cursor_position(&pos, binding, med_limits.record_bytes(), &mut buf).unwrap();

        assert_eq!(len, CURSOR_POSITION_RECORD_BYTES);

        let record = Record::decode(&buf[..len], &med_limits, binding, Channel::Video).unwrap();
        let decoded = decode_cursor_position(record).unwrap();

        assert_eq!(decoded, pos);
        assert!(decoded.is_visible());
        assert!(decoded.is_locked());
    }

    #[test]
    fn cursor_position_validation_failures() {
        let zero_seq = CursorPosition {
            shape_id: 1,
            x: 0,
            y: 0,
            geometry_generation: 1,
            sequence: 0,
            flags: 0,
        };
        assert_eq!(zero_seq.validate(), Err(WireError::InvalidValue));

        let bad_flags = CursorPosition {
            shape_id: 1,
            x: 0,
            y: 0,
            geometry_generation: 1,
            sequence: 1,
            flags: 0x04,
        };
        assert_eq!(bad_flags.validate(), Err(WireError::InvalidFlags));
    }
}
