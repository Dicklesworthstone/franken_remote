//! Parent-to-presenter remote cursor overlay on the private presentation pipe.
//! NOT a network record, decode request, visibility witness or input command:
//! the presenter composites the single client-rendered cursor over the last
//! decoded picture. Every field is validated before the owned allocation.
use super::{Error, HEADER_BYTES};
use crate::cursor::check_geometry;
use core::fmt;
use fr_core::limits::ProtocolLimits;

const HIDDEN: u8 = 0;
const MOVE: u8 = 1;
const SHAPE: u8 = 2;
const MOVE_BYTES: usize = 9;
/// Tag, position, dimensions, hotspot and the 4-byte RGBA8 length.
pub const SHAPE_PREFIX_BYTES: usize = 21;

/// Straight-alpha RGBA8 with checked geometry. Coordinates are the hotspot
/// in DECODED picture pixels; the presenter maps them into its own viewport.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Update<'a> {
    Hidden,
    Move {
        x: i32,
        y: i32,
    },
    Shape {
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        rgba: &'a [u8],
    },
}
impl fmt::Debug for Update<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hidden => f.write_str("Hidden"),
            Self::Move { .. } => f.write_str("Move"),
            Self::Shape {
                width,
                height,
                rgba,
                ..
            } => f
                .debug_struct("Shape")
                .field("width", width)
                .field("height", height)
                .field("bytes", &rgba.len())
                .finish_non_exhaustive(),
        }
    }
}
pub(super) fn accepts_length(len: usize, limits: &ProtocolLimits) -> bool {
    (len == 1 || len == MOVE_BYTES || len > SHAPE_PREFIX_BYTES)
        && len
            .checked_add(HEADER_BYTES)
            .is_some_and(|n| n <= limits.max_control_message_bytes() as usize)
}
pub fn encode(update: &Update<'_>, limits: &ProtocolLimits) -> Result<Vec<u8>, Error> {
    let (tag, x, y, shape) = match *update {
        Update::Hidden => return Ok(vec![HIDDEN]),
        Update::Move { x, y } => (MOVE, x, y, None),
        Update::Shape {
            x,
            y,
            width,
            height,
            hotspot_x,
            hotspot_y,
            rgba,
        } => {
            check_geometry(width, height, hotspot_x, hotspot_y, rgba.len())
                .map_err(|_| Error::ResourceLimit)?;
            (
                SHAPE,
                x,
                y,
                Some((width, height, hotspot_x, hotspot_y, rgba)),
            )
        }
    };
    let len = shape.map_or(MOVE_BYTES, |s| SHAPE_PREFIX_BYTES + s.4.len());
    if !accepts_length(len, limits) {
        return Err(Error::ResourceLimit);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(len).map_err(|_| Error::Allocation)?;
    out.push(tag);
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
    if let Some((width, height, hotspot_x, hotspot_y, rgba)) = shape {
        for n in [width, height, hotspot_x, hotspot_y] {
            out.extend_from_slice(&n.to_be_bytes());
        }
        out.extend_from_slice(
            &u32::try_from(rgba.len())
                .map_err(|_| Error::ResourceLimit)?
                .to_be_bytes(),
        );
        out.extend_from_slice(rgba);
    }
    Ok(out)
}
fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}
fn be32(b: &[u8], at: usize) -> [u8; 4] {
    [b[at], b[at + 1], b[at + 2], b[at + 3]]
}
/// Borrow the received body; malformed tags, lengths or geometry refuse.
pub fn decode<'a>(body: &'a [u8], limits: &ProtocolLimits) -> Result<Update<'a>, Error> {
    if !accepts_length(body.len(), limits) {
        return Err(Error::ResourceLimit);
    }
    match (body[0], body.len()) {
        (HIDDEN, 1) => Ok(Update::Hidden),
        (MOVE, MOVE_BYTES) => Ok(Update::Move {
            x: i32::from_be_bytes(be32(body, 1)),
            y: i32::from_be_bytes(be32(body, 5)),
        }),
        (SHAPE, n) if n > SHAPE_PREFIX_BYTES => {
            let rgba = &body[SHAPE_PREFIX_BYTES..];
            let declared = u32::from_be_bytes(be32(body, 17));
            if usize::try_from(declared).map_err(|_| Error::Malformed)? != rgba.len() {
                return Err(Error::Malformed);
            }
            let (width, height, hotspot_x, hotspot_y) = (
                be16(body, 9),
                be16(body, 11),
                be16(body, 13),
                be16(body, 15),
            );
            check_geometry(width, height, hotspot_x, hotspot_y, rgba.len())
                .map_err(|_| Error::Malformed)?;
            Ok(Update::Shape {
                x: i32::from_be_bytes(be32(body, 1)),
                y: i32::from_be_bytes(be32(body, 5)),
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            })
        }
        _ => Err(Error::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlay_updates_round_trip_and_refuse_hostile_geometry_before_allocation() {
        let limits = ProtocolLimits::ABSOLUTE;
        let rgba = [1_u8; 2 * 3 * 4];
        let shape = Update::Shape {
            x: -4,
            y: 7,
            width: 2,
            height: 3,
            hotspot_x: 1,
            hotspot_y: 2,
            rgba: &rgba,
        };
        for update in [Update::Hidden, Update::Move { x: 5, y: -6 }, shape] {
            let body = encode(&update, &limits).unwrap();
            assert_eq!(decode(&body, &limits).unwrap(), update);
        }
        assert!(!format!("{shape:?}").contains("1, 1"));
        let body = encode(&shape, &limits).unwrap();
        for n in 0..body.len() {
            assert!(decode(&body[..n], &limits).is_err(), "prefix {n}");
        }
        // Hotspot outside, zero size, length mismatch and unknown tag.
        for (offset, value) in [(13, 2_u8), (10, 0), (20, 99), (0, 9)] {
            let mut hostile = body.clone();
            hostile[offset] = value;
            assert!(decode(&hostile, &limits).is_err(), "offset {offset}");
        }
        for update in [
            Update::Shape {
                width: 0,
                height: 0,
                hotspot_x: 0,
                hotspot_y: 0,
                x: 0,
                y: 0,
                rgba: &[],
            },
            Update::Shape {
                width: 257,
                height: 1,
                hotspot_x: 0,
                hotspot_y: 0,
                x: 0,
                y: 0,
                rgba: &[0; 257 * 4],
            },
        ] {
            assert_eq!(encode(&update, &limits), Err(Error::ResourceLimit));
        }
    }
}
