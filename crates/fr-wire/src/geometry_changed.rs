//! Wire codec for `0x0021 GeometryChanged` (`PROTOCOL.md` §6).
//!
//! Host-to-viewer reliable control notification emitted after display geometry,
//! scale, origin, or rotation changes. Per `PROTOCOL.md` §6, the host MUST fence
//! old coordinate input BEFORE transmitting this record; `input_fenced` is
//! mandatory (`1`) on the wire, and `previous_geometry` must strictly precede
//! `display.geometry` so stale or no-op transitions are refused.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    display::Display,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    ids::{DisplayGeometryGeneration, ViewportMappingGeneration},
    limits::ProtocolLimits,
};

pub const CAPABILITY: &str = "display-geometry-events";
pub const VERSION: u16 = 1;
/// Fixed wire size including the 24-byte `FRD0` header:
/// 24 + 48 (`ControlBinding`) + 8 (revision) + 8 (`previous_geometry`) +
/// 57 (`Display`) + 8 (`viewport`) + 1 (`input_fenced`) = 154 bytes.
pub const RECORD_BYTES: usize = HEADER_BYTES + 48 + 8 + 8 + crate::display::ENTRY_BYTES + 8 + 1;

/// Checked display geometry transition with mandatory pre-send input fencing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GeometryChanged {
    /// Nonzero catalog revision matching the updated display catalog.
    pub revision: u64,
    /// Geometry generation fenced before sending this update.
    pub previous_geometry: DisplayGeometryGeneration,
    /// Replacement display entry with a strictly newer geometry generation.
    pub display: Display,
    /// New viewport mapping generation fencing prior coordinate mappings.
    pub viewport: ViewportMappingGeneration,
    /// Must be `true`; a host must fence old coordinate input before sending.
    pub input_fenced: bool,
}

impl fmt::Debug for GeometryChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redact coordinates/dimensions to keep display geometry out of logs.
        f.debug_struct("GeometryChanged")
            .field("revision", &self.revision)
            .field("previous_geometry", &self.previous_geometry)
            .field("geometry", &self.display.geometry)
            .field("viewport", &self.viewport)
            .field("input_fenced", &self.input_fenced)
            .finish_non_exhaustive()
    }
}

impl GeometryChanged {
    pub fn validate(&self, limits: &ProtocolLimits) -> Result<(), WireError> {
        if self.revision == 0 || self.viewport.as_raw() == 0 {
            return Err(WireError::InvalidBinding);
        }
        if !self.input_fenced {
            return Err(WireError::InvalidValue);
        }
        if self.display.geometry.as_raw() <= self.previous_geometry.as_raw() {
            return Err(WireError::InvalidValue);
        }
        self.display.validate(limits)
    }
}

fn parent_valid(binding: ControlBinding) -> Result<(), WireError> {
    if binding.id == 0
        || binding.host_boot.as_raw() == 0
        || binding.os_session.as_raw() == 0
        || binding.remote_session.as_raw() == 0
    {
        return Err(WireError::InvalidBinding);
    }
    Ok(())
}

fn role(direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    if direction != InputDirection::HostToViewer {
        return Err(WireError::WrongRole);
    }
    Ok(())
}

fn read_u128(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}

fn read_i32(r: &mut Reader<'_>) -> Result<i32, WireError> {
    Ok(i32::from_be_bytes(
        r.take(4)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}

pub fn encode(
    event: &GeometryChanged,
    binding: ControlBinding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    parent_valid(binding)?;
    role(direction, delivery)?;
    event.validate(limits)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.id,
        Kind::GeometryChanged,
        RECORD_BYTES - HEADER_BYTES,
    )?;
    for id in [
        binding.host_boot.as_raw(),
        binding.os_session.as_raw(),
        binding.remote_session.as_raw(),
    ] {
        w.put(&id.to_be_bytes())?;
    }
    w.u64(event.revision)?;
    w.u64(event.previous_geometry.as_raw())?;
    let d = event.display;
    w.put(&d.handle.to_be_bytes())?;
    w.u64(d.geometry.as_raw())?;
    w.put(&d.x.to_be_bytes())?;
    w.put(&d.y.to_be_bytes())?;
    for v in [
        d.pixel_width,
        d.pixel_height,
        d.logical_width,
        d.logical_height,
        d.scale_numerator,
        d.scale_denominator,
    ] {
        w.u32(v)?;
    }
    w.u8(d.rotation)?;
    w.u64(event.viewport.as_raw())?;
    w.u8(u8::from(event.input_fenced))?;
    w.finish()
}

pub fn decode(
    bytes: &[u8],
    binding: ControlBinding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<GeometryChanged, WireError> {
    parent_valid(binding)?;
    role(direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.id,
        None,
    )?;
    let mut r = record.reader(Kind::GeometryChanged)?;
    for expected in [
        binding.host_boot.as_raw(),
        binding.os_session.as_raw(),
        binding.remote_session.as_raw(),
    ] {
        if read_u128(&mut r)? != expected {
            return Err(WireError::InvalidBinding);
        }
    }
    let revision = r.u64()?;
    let previous_geometry = DisplayGeometryGeneration::from_raw(r.u64()?);
    let display = Display {
        handle: read_u128(&mut r)?,
        geometry: DisplayGeometryGeneration::from_raw(r.u64()?),
        x: read_i32(&mut r)?,
        y: read_i32(&mut r)?,
        pixel_width: r.u32()?,
        pixel_height: r.u32()?,
        logical_width: r.u32()?,
        logical_height: r.u32()?,
        scale_numerator: r.u32()?,
        scale_denominator: r.u32()?,
        rotation: r.u8()?,
    };
    let viewport = ViewportMappingGeneration::from_raw(r.u64()?);
    let input_fenced = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err(WireError::InvalidValue),
    };
    r.finish()?;
    let event = GeometryChanged {
        revision,
        previous_geometry,
        display,
        viewport,
        input_fenced,
    };
    event.validate(limits)?;
    Ok(event)
}
