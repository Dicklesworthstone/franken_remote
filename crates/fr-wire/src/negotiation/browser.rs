//! Browser WSS first-message authentication, using the ordinary FRD0
//! `ClientHello` record rather than a second magic or JSON wire protocol.
//!
//! This is transport authentication only. A parsed hello, nonce consumption,
//! or open WebSocket never grants observation, readiness, or input control.

use super::{Error, Offer};
use crate::{Kind, Record, record::Writer};
use core::fmt;

/// Dedicated-connection WSS with TCP head-of-line blocking. ID 2 remains
/// reserved for the separately qualified HTTP/3 WebTransport profile.
pub const WSS_PROFILE: u8 = 3;
/// WSS evolves independently of the native transport profile.
pub const PROFILE_VERSION: u16 = 0;
pub const NONCE_BYTES: usize = 32;
pub const MAX_RECORD: usize = super::MAX_RECORD;

/// A complete browser hello. Debug output deliberately omits the bearer nonce.
#[derive(Clone, PartialEq, Eq)]
pub struct Hello {
    pub offer: Offer,
    nonce: [u8; NONCE_BYTES],
}

impl fmt::Debug for Hello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserHello")
            .field("offer", &self.offer)
            .finish_non_exhaustive()
    }
}

impl Hello {
    pub fn new(offer: Offer, nonce: [u8; NONCE_BYTES]) -> Result<Self, Error> {
        let hello = Self { offer, nonce };
        hello.validate()?;
        Ok(hello)
    }

    /// Access only for encoding or atomic, peer-bound nonce consumption.
    pub const fn nonce(&self) -> &[u8; NONCE_BYTES] {
        &self.nonce
    }

    pub fn validate(&self) -> Result<(), Error> {
        let o = &self.offer;
        if o.profile != WSS_PROFILE || o.profile_version != PROFILE_VERSION {
            return Err(Error::Profile);
        }
        if o.versions.is_empty()
            || o.versions.len() > super::MAX_VERSIONS
            || o.versions.windows(2).any(|w| w[0] >= w[1])
        {
            return Err(Error::Version);
        }
        super::valid_limits(o.limits)?;
        super::valid_caps(&o.capabilities)
    }
}

/// Encode one zero-bound FRD0 `ClientHello`. The offer prefix is byte-for-byte
/// identical to native negotiation; `nonce_present=1` is followed by 32 bytes.
pub fn encode_hello(hello: &Hello, maximum: usize, out: &mut [u8]) -> Result<usize, Error> {
    hello.validate()?;
    let mut writer = Writer::record_bounded(
        out,
        maximum.min(MAX_RECORD),
        0,
        Kind::ClientHello,
        super::offer_bytes(&hello.offer) + NONCE_BYTES,
    )?;
    super::write_offer_fields(&mut writer, &hello.offer)?;
    writer.u8(1)?;
    writer.put(&hello.nonce)?;
    Ok(writer.finish()?)
}

/// Decode exactly one bounded zero-bound browser hello. Native/missing nonce,
/// unknown profile/version, duplicate capabilities, trailing bytes, and all
/// other ordinary FRD0 framing errors are rejected before authentication.
pub fn decode_hello(bytes: &[u8], maximum: usize) -> Result<Hello, Error> {
    let record = Record::decode_bounded(bytes, maximum.min(MAX_RECORD), 0, None)?;
    let mut reader = record.reader(Kind::ClientHello)?;
    let offer = super::read_offer_fields(&mut reader)?;
    if reader.u8()? != 1 {
        return Err(Error::Profile);
    }
    let nonce = reader
        .take(NONCE_BYTES)?
        .try_into()
        .map_err(|_| Error::Invalid)?;
    reader.finish()?;
    Hello::new(offer, nonce)
}
