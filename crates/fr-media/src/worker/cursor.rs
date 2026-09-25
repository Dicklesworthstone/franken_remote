//! Logical OS cursor observation on private, parent-owned capture pipes.
//! These are NOT network shape IDs, source-freshness evidence or visibility.
//! A qualified visibility/lock owner and normal observation admission are still
//! required before the parent publishes cursor state to any remote viewer.
use super::{Error, HEADER_BYTES};
use core::fmt;
use fr_core::limits::ProtocolLimits;
use fr_wire::cursor::{CursorShape, MAX_CURSOR_BYTES};

pub const PREFIX_BYTES: usize = 25;
pub const MAX_BYTES: usize = PREFIX_BYTES + MAX_CURSOR_BYTES;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Snapshot<'a> {
    /// Native token, scoped to the original X server lifetime. Never a wire ID.
    pub native_serial: u32,
    /// Hotspot position relative to the selected capture rectangle.
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    /// Straight-alpha RGBA8, with checked geometry and exact byte length.
    pub rgba: &'a [u8],
}
impl fmt::Debug for Snapshot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogicalCursorSnapshot")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish_non_exhaustive()
    }
}
impl Snapshot<'_> {
    fn validate(self, limits: &ProtocolLimits) -> Result<(), Error> {
        if self.width == 0 || self.height == 0 || self.x < 0 || self.y < 0 {
            return Err(Error::Malformed);
        }
        CursorShape {
            shape_id: 0,
            width: self.width,
            height: self.height,
            hotspot_x: self.hotspot_x,
            hotspot_y: self.hotspot_y,
            scale_1000: 1000,
            flags: 0, // logical image: visibility and lock are UNKNOWN
            rgba: self.rgba,
        }
        .validate(limits)
        .map_err(|_| Error::ResourceLimit)?;
        if !accepts_length(PREFIX_BYTES + self.rgba.len(), limits) {
            return Err(Error::ResourceLimit);
        }
        Ok(())
    }
}
pub(super) fn accepts_length(len: usize, limits: &ProtocolLimits) -> bool {
    (len == 1 || (PREFIX_BYTES..=MAX_BYTES).contains(&len))
        && len
            .checked_add(HEADER_BYTES)
            .is_some_and(|n| n <= limits.max_control_message_bytes() as usize)
}
/// One typed `ReadCursor` outcome. Only `Inside` carries an image; the other
/// states are non-fatal and never fabricate coordinates or pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation<'a> {
    /// A logical image with the pointer inside the selected capture scope.
    Inside(Snapshot<'a>),
    /// Pointer outside the selected capture scope (not a hidden-cursor claim).
    Outside,
    /// The pointer moved between the native image and position queries.
    Moving,
    /// The display cannot observe a separate cursor (e.g. no XFIXES).
    Unsupported,
    /// A cursor exists but cannot be represented within the admitted bounds.
    Unrepresentable,
}
const UNSUPPORTED: u8 = 2;
const UNREPRESENTABLE: u8 = 3;
/// Encode a typed observation. `Moving` is carried by the `NeedInput` reply
/// kind, never by this body.
pub fn encode_observation(
    observation: Observation<'_>,
    limits: &ProtocolLimits,
) -> Result<Vec<u8>, Error> {
    match observation {
        Observation::Inside(s) => encode(Some(s), limits),
        Observation::Outside => encode(None, limits),
        Observation::Unsupported => Ok(vec![UNSUPPORTED]),
        Observation::Unrepresentable => Ok(vec![UNREPRESENTABLE]),
        Observation::Moving => Err(Error::WrongState),
    }
}
/// Decode a `CursorSnapshot` body into its typed observation.
pub fn decode_observation<'a>(
    body: &'a [u8],
    limits: &ProtocolLimits,
) -> Result<Observation<'a>, Error> {
    match body {
        [UNSUPPORTED] => Ok(Observation::Unsupported),
        [UNREPRESENTABLE] => Ok(Observation::Unrepresentable),
        _ => Ok(decode(body, limits)?.map_or(Observation::Outside, Observation::Inside)),
    }
}
/// `None` means outside the selected capture scope, never a hidden-cursor claim.
/// A present logical image does not certify visibility: no visibility bit exists
/// in this private format. Encode bounds are checked before the owned allocation.
pub fn encode(snapshot: Option<Snapshot<'_>>, limits: &ProtocolLimits) -> Result<Vec<u8>, Error> {
    let Some(s) = snapshot else {
        return Ok(vec![0]);
    };
    s.validate(limits)?;
    let mut out = Vec::new();
    out.try_reserve_exact(PREFIX_BYTES + s.rgba.len())
        .map_err(|_| Error::Allocation)?;
    out.push(1);
    out.extend_from_slice(&s.native_serial.to_be_bytes());
    out.extend_from_slice(&s.x.to_be_bytes());
    out.extend_from_slice(&s.y.to_be_bytes());
    for n in [s.width, s.height, s.hotspot_x, s.hotspot_y] {
        out.extend_from_slice(&n.to_be_bytes());
    }
    out.extend_from_slice(
        &u32::try_from(s.rgba.len())
            .map_err(|_| Error::ResourceLimit)?
            .to_be_bytes(),
    );
    out.extend_from_slice(s.rgba);
    Ok(out)
}
/// Borrow the original response bytes; malformed lengths, tags and geometry
/// refuse without allocating another cursor image or consuming a network ID.
pub fn decode<'a>(body: &'a [u8], limits: &ProtocolLimits) -> Result<Option<Snapshot<'a>>, Error> {
    if !accepts_length(body.len(), limits) {
        return Err(Error::ResourceLimit);
    }
    if body == [0] {
        return Ok(None);
    }
    if body.len() < PREFIX_BYTES || body[0] != 1 {
        return Err(Error::Malformed);
    }
    let s = Snapshot {
        native_serial: u32::from_be_bytes(body[1..5].try_into().unwrap()),
        x: i32::from_be_bytes(body[5..9].try_into().unwrap()),
        y: i32::from_be_bytes(body[9..13].try_into().unwrap()),
        width: u16::from_be_bytes(body[13..15].try_into().unwrap()),
        height: u16::from_be_bytes(body[15..17].try_into().unwrap()),
        hotspot_x: u16::from_be_bytes(body[17..19].try_into().unwrap()),
        hotspot_y: u16::from_be_bytes(body[19..21].try_into().unwrap()),
        rgba: &body[PREFIX_BYTES..],
    };
    let len = u32::from_be_bytes(body[21..25].try_into().unwrap());
    if usize::try_from(len).map_err(|_| Error::ResourceLimit)? != s.rgba.len() {
        return Err(Error::Malformed);
    }
    s.validate(limits)?;
    Ok(Some(s))
}
