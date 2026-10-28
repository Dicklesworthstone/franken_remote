//! Capture-local bitrate requests and actual worker results. This is private
//! parent/worker IPC, not a new network codec configuration or capacity estimate.
//! The rate result occupies reserved bytes in a DISTINCT unit kind, preserving
//! the existing AU allocation ceiling and strict legacy reserved-byte checks.
use super::{Error, UNIT_PREFIX_BYTES, UnchangedCapture};
use crate::access_unit::{EncodedAccessUnit, FrameId};
use fr_core::limits::ProtocolLimits;

pub const CAPTURE_BYTES: usize = 21;
pub const UNCHANGED_BYTES: usize = 28;

fn valid_bitrate(bitrate: u32) -> Result<u32, Error> {
    if (10_000..=200_000_000).contains(&bitrate) {
        Ok(bitrate)
    } else {
        Err(Error::ResourceLimit)
    }
}

/// One original capture credit, candidate and pre-request source timestamp.
/// The worker may defer the target while idle or during its restart cooldown;
/// only the returned actual rate is evidence that a change was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capture {
    pub frame: FrameId,
    pub observed_micros: u64,
    pub force_idr: bool,
    pub only_if_changed: bool,
    pub bitrate: u32,
}
impl Capture {
    pub fn encode(self) -> Result<Vec<u8>, Error> {
        valid_bitrate(self.bitrate)?;
        let mut bytes = Vec::with_capacity(CAPTURE_BYTES);
        bytes.extend_from_slice(&self.frame.as_raw().to_be_bytes());
        bytes.extend_from_slice(&self.observed_micros.to_be_bytes());
        bytes.push(u8::from(self.force_idr) | (u8::from(self.only_if_changed) << 1));
        bytes.extend_from_slice(&self.bitrate.to_be_bytes());
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let b: &[u8; CAPTURE_BYTES] = bytes.try_into().map_err(|_| Error::Malformed)?;
        if b[16] & !3 != 0 {
            return Err(Error::Malformed);
        }
        Ok(Self {
            frame: FrameId::from_raw(u64::from_be_bytes(b[..8].try_into().unwrap())),
            observed_micros: u64::from_be_bytes(b[8..16].try_into().unwrap()),
            force_idr: b[16] & 1 != 0,
            only_if_changed: b[16] & 2 != 0,
            bitrate: valid_bitrate(u32::from_be_bytes(b[17..].try_into().unwrap()))?,
        })
    }
}

/// Use only with `Kind::RateUnit`. The rate describes the validated output,
/// never merely the target on the request which caused it.
pub fn unit_payload(unit: &EncodedAccessUnit, bitrate: u32) -> Result<Vec<u8>, Error> {
    valid_bitrate(bitrate)?;
    let mut body = super::unit_payload(unit)?;
    body[33..37].copy_from_slice(&bitrate.to_be_bytes());
    Ok(body)
}
pub fn parse_unit(
    mut body: Vec<u8>,
    limits: &ProtocolLimits,
) -> Result<(u32, EncodedAccessUnit), Error> {
    if body.len() <= UNIT_PREFIX_BYTES || body[37..40] != [0; 3] {
        return Err(Error::Malformed);
    }
    let bitrate = valid_bitrate(u32::from_be_bytes(body[33..37].try_into().unwrap()))?;
    body[33..37].fill(0);
    // Reuse the original allocation and all ordinary frame/config/reference
    // validation. No second AU buffer or uncharged four-byte prefix is added.
    Ok((bitrate, super::parse_unit(body, limits)?))
}
pub fn unchanged_payload(proof: UnchangedCapture, bitrate: u32) -> Result<Vec<u8>, Error> {
    valid_bitrate(bitrate)?;
    let encoded = proof.encode()?;
    let mut body = Vec::with_capacity(UNCHANGED_BYTES);
    body.extend_from_slice(&encoded);
    body.extend_from_slice(&bitrate.to_be_bytes());
    Ok(body)
}
pub fn parse_unchanged(body: &[u8]) -> Result<(u32, UnchangedCapture), Error> {
    let b: &[u8; UNCHANGED_BYTES] = body.try_into().map_err(|_| Error::Malformed)?;
    Ok((
        valid_bitrate(u32::from_be_bytes(b[24..].try_into().unwrap()))?,
        UnchangedCapture::decode(&b[..24])?,
    ))
}
