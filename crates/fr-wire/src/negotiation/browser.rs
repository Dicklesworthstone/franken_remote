//! Browser WSS first-message authentication, using the ordinary FRD0
//! `ClientHello` record rather than a second magic or JSON wire protocol.
//!
//! This is transport authentication only. A parsed hello, nonce consumption,
//! or open WebSocket never grants observation, readiness, or input control.

use super::{Capability, Error, Message, Offer, Role, Selection};
use fr_core::limits::{LimitOverrides, ProtocolLimits};

pub mod attachment;
use crate::{Kind, Record, record::Writer};
use core::fmt;

/// Dedicated-connection WSS with TCP head-of-line blocking. ID 2 remains
/// reserved for the separately qualified HTTP/3 WebTransport profile.
pub const WSS_PROFILE: u8 = 3;
/// WSS evolves independently of the native transport profile.
pub const PROFILE_VERSION: u16 = 0;
pub const NONCE_BYTES: usize = 32;
pub const MAX_RECORD: usize = super::MAX_RECORD;

/// Optional support for one attributed host-local approval before observation.
/// A host requiring consent marks this capability required in its offer.
pub const APPROVAL_CAPABILITY: &str = "browser-local-approval";
pub const APPROVAL_VERSION: u16 = 1;
/// Total startup ceiling measured from the ORIGINAL attempt/control start.
/// A valid approval notice selects this bound; receipt never restarts it.
pub const APPROVAL_STARTUP_BUDGET_US: u64 = 30_000_000;

pub fn local_approval_selected(selection: &Selection) -> bool {
    selection.role == Role::Observe
        && selection.validate_for(WSS_PROFILE).is_ok()
        && selection.capabilities.iter().any(|capability| {
            capability.name == APPROVAL_CAPABILITY && capability.version == APPROVAL_VERSION
        })
}

/// The selected host policy requires an original approval notice before open.
/// Merely supporting optional consent does not turn it on for another host.
pub fn local_approval_required(selection: &Selection) -> bool {
    local_approval_selected(selection)
        && selection
            .capabilities
            .iter()
            .any(|capability| capability.name == APPROVAL_CAPABILITY && capability.required)
}

/// One initial full-display choice on the approved browser control binding.
pub fn display_selection_selected(selection: &Selection) -> bool {
    selection.role == Role::Observe
        && selection.validate_for(WSS_PROFILE).is_ok()
        && selection.capabilities.iter().any(|capability| {
            capability.name == crate::display::CAPABILITY
                && capability.version == crate::display::VERSION
        })
}

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
        self.offer.validate_for(WSS_PROFILE)
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

/// Explicitly multiplexes canonical configuration, recovery and video records
/// on one dedicated media socket. Control/observation/clock/credit stay separate.
pub const VIDEO_CAPABILITY: &str = "browser-video-wss";
pub const VIDEO_VERSION: u16 = 1;
pub const MAX_VIDEO_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_VIDEO_CREDIT_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_VIDEO_CREDIT_FRAMES: u32 = 4;
pub const FRAME_METADATA_BYTES: u64 = 4096;
#[allow(
    clippy::cast_possible_truncation,
    reason = "The fixed 2 MiB ceiling fits u32."
)]
pub const MAX_ACCESS_UNIT_BYTES: u32 = ((MAX_VIDEO_CREDIT_BYTES - FRAME_METADATA_BYTES) / 3) as u32;

/// Reserve the receive allocation and both overlapping copies at the WASM/JS
/// and `WebCodecs` handoffs, plus frame metadata, before the first chunk. The
/// complete reservation remains charged until original receive/native disposal.
pub fn frame_charge(encoded_bytes: u32) -> Result<u64, crate::WireError> {
    if encoded_bytes == 0 || encoded_bytes > MAX_ACCESS_UNIT_BYTES {
        return Err(crate::WireError::ResourceLimit);
    }
    Ok(u64::from(encoded_bytes) * 3 + FRAME_METADATA_BYTES)
}
pub fn limits() -> ProtocolLimits {
    ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(
            u32::try_from(MAX_VIDEO_RECORD_BYTES).expect("fixed 64 KiB record bound"),
        ),
        max_encoded_access_unit_bytes: Some(MAX_ACCESS_UNIT_BYTES),
        per_viewer_compressed_bytes: Some(MAX_VIDEO_CREDIT_BYTES),
        reassembly_window_pictures: Some(
            u8::try_from(MAX_VIDEO_CREDIT_FRAMES).expect("fixed four-picture bound"),
        ),
        ..Default::default()
    })
    .expect("fixed browser profile is within protocol limits")
}
/// A view-only offer. A control request has no implicit observation fallback.
pub fn offer(ceiling: ProtocolLimits) -> Offer {
    Offer {
        versions: vec![0],
        profile: WSS_PROFILE,
        profile_version: PROFILE_VERSION,
        role: Role::Observe,
        limits: limits().negotiated(&ceiling),
        capabilities: vec![
            Capability {
                name: APPROVAL_CAPABILITY.to_owned(),
                version: APPROVAL_VERSION,
                required: false,
            },
            Capability {
                name: VIDEO_CAPABILITY.to_owned(),
                version: VIDEO_VERSION,
                required: true,
            },
            Capability {
                name: crate::clock::CAPABILITY.to_owned(),
                version: crate::clock::VERSION,
                required: true,
            },
            Capability {
                name: crate::display::CAPABILITY.to_owned(),
                version: crate::display::VERSION,
                required: false,
            },
            Capability {
                name: crate::decoder::CAPABILITY.to_owned(),
                version: crate::decoder::VERSION,
                required: true,
            },
        ],
    }
}
pub fn intersect(host: &Offer, peer: &Offer) -> Result<Offer, Error> {
    host.intersect_for(peer, WSS_PROFILE)
}
pub fn select(offer: &Offer) -> Result<Selection, Error> {
    offer.select_picture_for(None, WSS_PROFILE)
}
pub fn check_host(original: &Offer, host: &Offer) -> Result<(), Error> {
    original.check_host_for(host, WSS_PROFILE)
}
pub fn check_selection(selection: &Selection, offer: &Offer) -> Result<(), Error> {
    selection.check_against_for(offer, WSS_PROFILE)
}
/// `ClientHello` MUST use the nonce-bearing encoder. Subsequent negotiation uses
/// the same canonical codec while preserving the native API's strict profile.
pub fn encode(message: &Message, maximum: usize, out: &mut [u8]) -> Result<usize, Error> {
    if matches!(message, Message::ClientHello(_)) {
        return Err(Error::Profile);
    }
    super::encode_for(message, maximum, out, WSS_PROFILE)
}
pub fn decode(bytes: &[u8], maximum: usize, expected_binding: u32) -> Result<Message, Error> {
    let message = super::decode_for(bytes, maximum, expected_binding, WSS_PROFILE)?;
    if matches!(message, Message::ClientHello(_)) {
        return Err(Error::Profile);
    }
    Ok(message)
}
/// Bounded canonical envelope inspection for dispatch only. Every selected
/// message still requires its role-specific schema and full original binding.
pub fn record_kind(bytes: &[u8], maximum: usize, binding: u32) -> Result<Kind, crate::WireError> {
    Record::inspect_bounded(bytes, maximum.min(MAX_VIDEO_RECORD_BYTES), binding)
}
