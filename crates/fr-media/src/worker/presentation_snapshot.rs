//! An explicit copy of the retained, composited presentation viewport.
//! This is private worker IPC: no desktop recapture, decoder advancement,
//! visibility witness or input authority. Pixel bytes never appear in Debug.
use super::Error;
use crate::access_unit::FrameId;
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};

pub const STAMP_BYTES: usize = 24;
pub const HEADER_BYTES: usize = STAMP_BYTES + 8;
pub const MAX_PIXEL_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_BODY_BYTES: usize = HEADER_BYTES + MAX_PIXEL_BYTES;

/// Identity of the video picture underlying the retained viewport. Cursor
/// composition may change without advancing this picture or its generations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub frame: FrameId,
    pub configuration: CodecConfigurationGeneration,
    pub recovery: RecoveryGeneration,
}
impl Stamp {
    pub fn encode(self) -> [u8; STAMP_BYTES] {
        let mut bytes = [0; STAMP_BYTES];
        bytes[..8].copy_from_slice(&self.frame.as_raw().to_be_bytes());
        bytes[8..16].copy_from_slice(&self.configuration.as_raw().to_be_bytes());
        bytes[16..].copy_from_slice(&self.recovery.as_raw().to_be_bytes());
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let b: &[u8; STAMP_BYTES] = bytes.try_into().map_err(|_| Error::Malformed)?;
        Ok(Self {
            frame: FrameId::from_raw(u64::from_be_bytes(b[..8].try_into().unwrap())),
            configuration: CodecConfigurationGeneration::from_raw(u64::from_be_bytes(
                b[8..16].try_into().unwrap(),
            )),
            recovery: RecoveryGeneration::from_raw(u64::from_be_bytes(b[16..].try_into().unwrap())),
        })
    }
}

/// Routine absence is a normal response, never the terminal worker Refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Absent {
    NoFrame = 1,
    NotCurrent = 2,
    Busy = 3,
    TooLarge = 4,
}
impl Absent {
    pub const fn encode(self) -> [u8; 1] {
        [self as u8]
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        match bytes {
            [1] => Ok(Self::NoFrame),
            [2] => Ok(Self::NotCurrent),
            [3] => Ok(Self::Busy),
            [4] => Ok(Self::TooLarge),
            _ => Err(Error::Malformed),
        }
    }
}

pub(super) fn accepts_length(length: usize) -> bool {
    length
        .checked_sub(HEADER_BYTES)
        .is_some_and(|pixels| pixels > 0 && pixels <= MAX_PIXEL_BYTES && pixels.is_multiple_of(4))
}

/// A separate export ceiling; ordinary control and encoded-picture limits
/// retain their original bounds. Tightly packed rows carry no native padding.
pub fn pixel_bytes(width: u32, height: u32) -> Result<usize, Error> {
    let bytes = ProtocolLimits::ABSOLUTE
        .checked_surface_bytes(width, height, 4, 1)
        .map_err(|_| Error::ResourceLimit)?;
    let bytes = usize::try_from(bytes).map_err(|_| Error::ResourceLimit)?;
    if bytes > MAX_PIXEL_BYTES {
        return Err(Error::ResourceLimit);
    }
    Ok(bytes)
}

/// Owns the original bounded response allocation, including its small header.
/// Dimensions name the presented viewport, including letterboxing and current
/// remote cursor composition. They need not equal the remote source dimensions.
/// Deliberately no Debug implementation over screen content.
pub struct Snapshot {
    stamp: Stamp,
    width: u32,
    height: u32,
    body: Vec<u8>,
}
impl Snapshot {
    /// Copy borrowed retained pixels once into one bounded response allocation.
    pub fn encode(stamp: Stamp, width: u32, height: u32, pixels: &[u8]) -> Result<Vec<u8>, Error> {
        let length = pixel_bytes(width, height)?;
        if pixels.len() != length {
            return Err(Error::Malformed);
        }
        let mut body = Vec::new();
        body.try_reserve_exact(HEADER_BYTES + length)
            .map_err(|_| Error::Allocation)?;
        if body.capacity() > MAX_BODY_BYTES {
            return Err(Error::ResourceLimit);
        }
        body.extend_from_slice(&stamp.encode());
        body.extend_from_slice(&width.to_be_bytes());
        body.extend_from_slice(&height.to_be_bytes());
        body.extend_from_slice(pixels);
        Ok(body)
    }
    /// Validate before exposing pixels; reuse the caller's existing allocation.
    pub fn decode(body: Vec<u8>) -> Result<Self, Error> {
        if !accepts_length(body.len()) || body.capacity() > MAX_BODY_BYTES {
            return Err(Error::ResourceLimit);
        }
        let stamp = Stamp::decode(&body[..STAMP_BYTES])?;
        let width = u32::from_be_bytes(body[24..28].try_into().unwrap());
        let height = u32::from_be_bytes(body[28..32].try_into().unwrap());
        if pixel_bytes(width, height)? != body.len() - HEADER_BYTES {
            return Err(Error::Malformed);
        }
        Ok(Self {
            stamp,
            width,
            height,
            body,
        })
    }
    pub const fn stamp(&self) -> Stamp {
        self.stamp
    }
    pub const fn width(&self) -> u32 {
        self.width
    }
    pub const fn height(&self) -> u32 {
        self.height
    }
    pub fn pixels(&self) -> &[u8] {
        &self.body[HEADER_BYTES..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{Header, Identity, Kind, Record};

    fn stamp() -> Stamp {
        Stamp {
            frame: FrameId::from_raw(17),
            configuration: CodecConfigurationGeneration::from_raw(3),
            recovery: RecoveryGeneration::from_raw(5),
        }
    }
    #[test]
    fn snapshot_owns_the_original_allocation_and_exact_viewport_bytes() {
        let pixels = [1, 2, 3, 255, 4, 5, 6, 255];
        let body = Snapshot::encode(stamp(), 2, 1, &pixels).unwrap();
        let pointer = body.as_ptr();
        let image = Snapshot::decode(body).unwrap();
        assert_eq!(image.stamp(), stamp());
        assert_eq!((image.width(), image.height()), (2, 1));
        assert_eq!(image.pixels(), pixels);
        assert_eq!(image.body.as_ptr(), pointer);
        assert_eq!(Stamp::decode(&stamp().encode()).unwrap(), stamp());
    }
    #[test]
    fn malformed_geometry_and_inexact_pixel_lengths_never_become_images() {
        assert!(matches!(
            Snapshot::encode(stamp(), 0, 1, &[]),
            Err(Error::ResourceLimit)
        ));
        assert!(matches!(
            Snapshot::encode(stamp(), u32::MAX, 2, &[]),
            Err(Error::ResourceLimit)
        ));
        assert!(matches!(
            Snapshot::encode(stamp(), 2, 2, &[0; 12]),
            Err(Error::Malformed)
        ));
        assert!(matches!(pixel_bytes(4096, 2160), Err(Error::ResourceLimit)));
        assert_eq!(pixel_bytes(3840, 2160).unwrap(), 33_177_600);
        let mut body = Snapshot::encode(stamp(), 2, 1, &[0; 8]).unwrap();
        body[28..32].copy_from_slice(&2_u32.to_be_bytes());
        assert!(matches!(Snapshot::decode(body), Err(Error::Malformed)));
        assert!(matches!(
            Snapshot::decode(vec![0; HEADER_BYTES]),
            Err(Error::ResourceLimit)
        ));
        assert_eq!(Stamp::decode(&[0; STAMP_BYTES - 1]), Err(Error::Malformed));
    }
    #[test]
    fn only_pixel_reply_headers_receive_the_independent_export_ceiling() {
        let limits = ProtocolLimits::ABSOLUTE;
        let identity = Identity {
            epoch: 1,
            sequence: 0,
        };
        let header = |kind, length| Header {
            kind,
            identity,
            length,
        };
        assert!(
            header(Kind::PresentationPixels, MAX_BODY_BYTES)
                .encode(&limits)
                .is_ok()
        );
        for length in [HEADER_BYTES, HEADER_BYTES + 1, MAX_BODY_BYTES + 4] {
            assert!(
                header(Kind::PresentationPixels, length)
                    .encode(&limits)
                    .is_err()
            );
        }
        for kind in [
            Kind::Unit,
            Kind::CursorSnapshot,
            Kind::CursorOverlay,
            Kind::Ready,
        ] {
            assert!(header(kind, MAX_BODY_BYTES).encode(&limits).is_err());
        }
        assert!(
            Record::new(
                Kind::ReadPresentation,
                identity,
                stamp().encode().to_vec(),
                &limits
            )
            .is_ok()
        );
        assert!(
            header(Kind::ReadPresentation, STAMP_BYTES + 1)
                .encode(&limits)
                .is_err()
        );
        for reason in [
            Absent::NoFrame,
            Absent::NotCurrent,
            Absent::Busy,
            Absent::TooLarge,
        ] {
            assert_eq!(Absent::decode(&reason.encode()).unwrap(), reason);
            assert!(
                Record::new(
                    Kind::PresentationAbsent,
                    identity,
                    reason.encode().to_vec(),
                    &limits
                )
                .is_ok()
            );
        }
        assert_eq!(Absent::decode(&[0]), Err(Error::Malformed));
        assert_eq!(Absent::decode(&[1, 0]), Err(Error::Malformed));
    }
}
