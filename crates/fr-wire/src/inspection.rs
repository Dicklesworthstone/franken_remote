//! Terminal capability inspection before selection or observation authority.
//! This extension is usable only on the authenticated native bootstrap pair,
//! after its required marker appears in both offers. See `PROTOCOL_INSPECTION.md`.
//! Ordinary session closure continues to require a nonzero session binding.
use crate::{
    HEADER_BYTES, Kind, WireError,
    negotiation::{self, Capability, Offer, Role},
};

pub const CAPABILITY: &str = "native-capability-inspection";
pub const VERSION: u16 = 1;
/// Includes all parked time from the original startup owner's creation.
pub const MAX_DURATION_US: u64 = 2_000_000;
pub const REQUEST_BYTES: usize = crate::closure::REQUEST_BYTES;
pub const CLOSED_BYTES: usize = crate::closure::CLOSED_BYTES;

/// Prepare an explicit terminal inspection. Control intent is never downgraded,
/// and an unsupported host must refuse this required marker rather than open.
pub fn prepare_offer(mut offer: Offer) -> Result<Offer, negotiation::Error> {
    offer.validate()?;
    if offer.role != Role::Observe {
        return Err(negotiation::Error::Invalid);
    }
    if let Some(marker) = offer.capabilities.iter().find(|c| c.name == CAPABILITY) {
        return if marker.version == VERSION && marker.required {
            Ok(offer)
        } else {
            Err(negotiation::Error::RequiredCapability)
        };
    }
    if offer.capabilities.len() >= negotiation::MAX_CAPABILITIES {
        return Err(negotiation::Error::Invalid);
    }
    offer
        .capabilities
        .try_reserve_exact(1)
        .map_err(|_| negotiation::Error::Allocation)?;
    offer.capabilities.push(Capability {
        name: CAPABILITY.into(),
        version: VERSION,
        required: true,
    });
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer.validate()?;
    Ok(offer)
}

/// Caller must also validate the complete offer. A name alone is not agreement.
pub fn requested(offer: &Offer) -> bool {
    offer.role == Role::Observe
        && offer
            .capabilities
            .iter()
            .any(|c| c.name == CAPABILITY && c.version == VERSION && c.required)
}

/// The host's implemented intersection for this terminal extension. The marker
/// occupies one of the existing 24 slots; a full normal host offer needs no
/// extra slot and normal session negotiation does not advertise this extension.
pub fn intersect(host: &Offer, peer: &Offer) -> Result<Offer, negotiation::Error> {
    host.validate()?;
    peer.validate()?;
    if !requested(peer) {
        return Err(negotiation::Error::RequiredCapability);
    }
    let mut diagnostic = peer.clone();
    diagnostic.capabilities.retain(|c| c.name != CAPABILITY);
    let result = host.intersect(&diagnostic)?;
    prepare_offer(result)
}

// Exact templates deliberately do not broaden Record::decode or closure's
// zero-binding policy. No extension, unknown reason, effect count, session
// identifier, or alternate scope can enter this bootstrap-only terminal form.
const REQUEST: [u8; REQUEST_BYTES] = {
    let mut bytes = header::<REQUEST_BYTES>(Kind::CloseRequest, 19);
    bytes[HEADER_BYTES + 16] = 1; // Terminal connection scope; no session exists.
    let reason = (crate::closure::Reason::InspectionComplete as u16).to_be_bytes();
    bytes[HEADER_BYTES + 17] = reason[0];
    bytes[HEADER_BYTES + 18] = reason[1];
    bytes
};
const CLOSED: [u8; CLOSED_BYTES] = {
    let mut bytes = header::<CLOSED_BYTES>(Kind::Closed, 28);
    let reason = (crate::closure::ClosedReason::ClientRequested as u16).to_be_bytes();
    bytes[HEADER_BYTES + 16] = reason[0];
    bytes[HEADER_BYTES + 17] = reason[1];
    bytes[HEADER_BYTES + 18] = crate::closure::Cleanup::Complete as u8;
    bytes[HEADER_BYTES + 19] = 1; // Known zero effects: no session was authorized.
    bytes
};
const fn header<const N: usize>(kind: Kind, payload: u8) -> [u8; N] {
    let mut bytes = [0; N];
    bytes[0] = b'F';
    bytes[1] = b'R';
    bytes[2] = b'D';
    bytes[3] = b'0';
    let kind = (kind as u16).to_be_bytes();
    bytes[6] = kind[0];
    bytes[7] = kind[1];
    bytes[15] = payload;
    bytes
}
fn encode<const N: usize>(
    bytes: &[u8; N],
    maximum: usize,
    out: &mut [u8],
) -> Result<usize, WireError> {
    if N > maximum.min(negotiation::MAX_RECORD) {
        return Err(WireError::ResourceLimit);
    }
    let out = out.get_mut(..N).ok_or(WireError::BufferTooSmall)?;
    out.copy_from_slice(bytes);
    Ok(N)
}
fn decode<const N: usize>(
    bytes: &[u8],
    maximum: usize,
    expected: &[u8; N],
) -> Result<(), WireError> {
    if bytes.len() > maximum.min(negotiation::MAX_RECORD) {
        return Err(WireError::ResourceLimit);
    }
    if bytes != expected {
        return Err(WireError::InvalidValue);
    }
    Ok(())
}

/// Viewer-to-host only, on the original reliable bootstrap control stream.
pub fn encode_request(maximum: usize, out: &mut [u8]) -> Result<usize, WireError> {
    encode(&REQUEST, maximum, out)
}
pub fn decode_request(bytes: &[u8], maximum: usize) -> Result<(), WireError> {
    decode(bytes, maximum, &REQUEST)
}

/// Host-to-viewer only, after permanently fencing the unselected connection.
pub fn encode_closed(maximum: usize, out: &mut [u8]) -> Result<usize, WireError> {
    encode(&CLOSED, maximum, out)
}
pub fn decode_closed(bytes: &[u8], maximum: usize) -> Result<(), WireError> {
    decode(bytes, maximum, &CLOSED)
}
