//! Bounded full-display selection on an already approved session. Handles are
//! host-owned aliases for local display identities, never OS paths or commands.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};

pub const CAPABILITY: &str = "display-selection";
pub const VERSION: u16 = 1;
pub const MAX_DISPLAYS: usize = 8;
pub const ENTRY_BYTES: usize = 57;
pub const CATALOG_BASE_BYTES: usize = HEADER_BYTES + 57;
pub const MAX_CATALOG_BYTES: usize = CATALOG_BASE_BYTES + MAX_DISPLAYS * ENTRY_BYTES;
pub const SELECT_BYTES: usize = HEADER_BYTES + 96;

/// Dimensions and origins describe the already rotated desktop coordinate
/// space. Scale is an explicit rational, not an inferred DPI or input mapping.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Display {
    pub handle: u128,
    pub geometry: DisplayGeometryGeneration,
    pub x: i32,
    pub y: i32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub scale_numerator: u32,
    pub scale_denominator: u32,
    /// Clockwise quarter turns, in 0..=3. Dimensions are post-rotation.
    pub rotation: u8,
}
impl fmt::Debug for Display {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Display([redacted])")
    }
}
impl Display {
    const EMPTY: Self = Self {
        handle: 0,
        geometry: DisplayGeometryGeneration::INITIAL,
        x: 0,
        y: 0,
        pixel_width: 0,
        pixel_height: 0,
        logical_width: 0,
        logical_height: 0,
        scale_numerator: 0,
        scale_denominator: 0,
        rotation: 0,
    };
    pub fn validate(self, limits: &ProtocolLimits) -> Result<(), WireError> {
        if self.handle == 0 {
            return Err(WireError::InvalidBinding);
        }
        for (w, h) in [
            (self.pixel_width, self.pixel_height),
            (self.logical_width, self.logical_height),
        ] {
            limits
                .validate_coded_dimensions(w, h)
                .map_err(|_| WireError::ResourceLimit)?;
        }
        // Validate the last addressable pixel, not the exclusive upper edge:
        // an origin at i32::MAX with one pixel is a valid desktop coordinate.
        for (origin, size) in [(self.x, self.pixel_width), (self.y, self.pixel_height)] {
            let last = i64::from(origin) + i64::from(size) - 1;
            i32::try_from(last).map_err(|_| WireError::ArithmeticOverflow)?;
        }
        let (n, d) = (self.scale_numerator, self.scale_denominator);
        if !(1..=65_536).contains(&n)
            || !(1..=65_536).contains(&d)
            || u64::from(n) > 16 * u64::from(d)
            || u64::from(d) > 16 * u64::from(n)
            || self.rotation > 3
        {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}

/// Fixed metadata storage, including on malicious count fields. An empty
/// catalog is meaningful: no display in the approved disclosure scope.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Catalog {
    revision: u64,
    count: u8,
    displays: [Display; MAX_DISPLAYS],
}
impl fmt::Debug for Catalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DisplayCatalog")
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}
impl Catalog {
    pub fn new(
        revision: u64,
        entries: &[Display],
        limits: &ProtocolLimits,
    ) -> Result<Self, WireError> {
        if revision == 0 {
            return Err(WireError::InvalidValue);
        }
        if entries.len() > MAX_DISPLAYS {
            return Err(WireError::ResourceLimit);
        }
        for (i, display) in entries.iter().enumerate() {
            display.validate(limits)?;
            if entries[..i]
                .iter()
                .any(|other| other.handle == display.handle)
            {
                return Err(WireError::InvalidBinding);
            }
        }
        let mut displays = [Display::EMPTY; MAX_DISPLAYS];
        displays[..entries.len()].copy_from_slice(entries);
        Ok(Self {
            revision,
            count: u8::try_from(entries.len()).map_err(|_| WireError::ResourceLimit)?,
            displays,
        })
    }
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    pub fn displays(&self) -> &[Display] {
        &self.displays[..usize::from(self.count)]
    }
    pub fn find(&self, handle: u128) -> Option<Display> {
        self.displays().iter().copied().find(|d| d.handle == handle)
    }
    pub fn selection(&self, handle: u128) -> Result<Select, WireError> {
        let d = self.find(handle).ok_or(WireError::InvalidBinding)?;
        Ok(Select {
            revision: self.revision,
            handle,
            geometry: d.geometry,
            x: 0,
            y: 0,
            width: d.pixel_width,
            height: d.pixel_height,
        })
    }
    /// The v1 capability selects a whole display. A viewport expansion or crop
    /// needs a separately qualified profile, never silent coordinate clamping.
    pub fn selected(&self, request: Select) -> Result<Display, WireError> {
        if request.revision != self.revision {
            return Err(WireError::InvalidBinding);
        }
        let d = self.find(request.handle).ok_or(WireError::InvalidBinding)?;
        if request != self.selection(d.handle)? {
            return Err(WireError::InvalidValue);
        }
        Ok(d)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Select {
    pub revision: u64,
    pub handle: u128,
    pub geometry: DisplayGeometryGeneration,
    /// Unsigned pixel rectangle relative to the selected display, not desktop.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
impl fmt::Debug for Select {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SelectDisplay([redacted])")
    }
}
// Keep a bounded catalog inline; boxing would introduce allocation on hostile input.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    Catalog(Catalog),
    Select(Select),
}
impl Message {
    pub const fn kind(&self) -> Kind {
        match self {
            Self::Catalog(_) => Kind::DisplayCatalog,
            Self::Select(_) => Kind::SelectDisplay,
        }
    }
}
fn parent_valid(b: ControlBinding) -> Result<(), WireError> {
    if b.id == 0
        || b.host_boot.as_raw() == 0
        || b.os_session.as_raw() == 0
        || b.remote_session.as_raw() == 0
    {
        return Err(WireError::InvalidBinding);
    }
    Ok(())
}
fn valid(m: &Message, limits: &ProtocolLimits) -> Result<(), WireError> {
    match m {
        Message::Catalog(c) => {
            Catalog::new(c.revision, c.displays(), limits)?;
        }
        Message::Select(s) => {
            if s.revision == 0 || s.handle == 0 {
                return Err(WireError::InvalidBinding);
            }
            limits
                .validate_coded_dimensions(s.width, s.height)
                .map_err(|_| WireError::ResourceLimit)?;
            s.x.checked_add(s.width)
                .ok_or(WireError::ArithmeticOverflow)?;
            s.y.checked_add(s.height)
                .ok_or(WireError::ArithmeticOverflow)?;
        }
    }
    Ok(())
}
fn role(kind: Kind, direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    let wanted = match kind {
        Kind::DisplayCatalog => InputDirection::HostToViewer,
        Kind::SelectDisplay => InputDirection::ViewerToHost,
        _ => return Err(WireError::UnsupportedKind),
    };
    if direction != wanted {
        return Err(WireError::WrongRole);
    }
    Ok(())
}
fn id(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}
fn signed(r: &mut Reader<'_>) -> Result<i32, WireError> {
    Ok(i32::from_be_bytes(
        r.take(4)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}
pub fn encode(
    m: &Message,
    b: ControlBinding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    parent_valid(b)?;
    role(m.kind(), direction, delivery)?;
    valid(m, limits)?;
    let len = match m {
        Message::Catalog(c) => CATALOG_BASE_BYTES + usize::from(c.count) * ENTRY_BYTES,
        Message::Select(_) => SELECT_BYTES,
    };
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        b.id,
        m.kind(),
        len - HEADER_BYTES,
    )?;
    for v in [
        b.host_boot.as_raw(),
        b.os_session.as_raw(),
        b.remote_session.as_raw(),
    ] {
        w.put(&v.to_be_bytes())?;
    }
    match m {
        Message::Catalog(c) => {
            w.u64(c.revision)?;
            w.u8(c.count)?;
            for d in c.displays() {
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
            }
        }
        Message::Select(s) => {
            w.u64(s.revision)?;
            w.put(&s.handle.to_be_bytes())?;
            w.u64(s.geometry.as_raw())?;
            for v in [s.x, s.y, s.width, s.height] {
                w.u32(v)?;
            }
        }
    }
    w.finish()
}
pub fn decode(
    bytes: &[u8],
    b: ControlBinding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Message, WireError> {
    parent_valid(b)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        b.id,
        None,
    )?;
    role(record.kind(), direction, delivery)?;
    let mut r = record.reader(record.kind())?;
    for v in [
        b.host_boot.as_raw(),
        b.os_session.as_raw(),
        b.remote_session.as_raw(),
    ] {
        if id(&mut r)? != v {
            return Err(WireError::InvalidBinding);
        }
    }
    let revision = r.u64()?;
    let m = if record.kind() == Kind::DisplayCatalog {
        let count = usize::from(r.u8()?);
        if count > MAX_DISPLAYS {
            return Err(WireError::ResourceLimit);
        }
        let mut displays = [Display::EMPTY; MAX_DISPLAYS];
        for d in &mut displays[..count] {
            *d = Display {
                handle: id(&mut r)?,
                geometry: DisplayGeometryGeneration::from_raw(r.u64()?),
                x: signed(&mut r)?,
                y: signed(&mut r)?,
                pixel_width: r.u32()?,
                pixel_height: r.u32()?,
                logical_width: r.u32()?,
                logical_height: r.u32()?,
                scale_numerator: r.u32()?,
                scale_denominator: r.u32()?,
                rotation: r.u8()?,
            };
        }
        Message::Catalog(Catalog::new(revision, &displays[..count], limits)?)
    } else {
        Message::Select(Select {
            revision,
            handle: id(&mut r)?,
            geometry: DisplayGeometryGeneration::from_raw(r.u64()?),
            x: r.u32()?,
            y: r.u32()?,
            width: r.u32()?,
            height: r.u32()?,
        })
    };
    r.finish()?;
    valid(&m, limits)?;
    Ok(m)
}
