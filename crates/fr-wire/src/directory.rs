//! Terminal tailnet directory on the authenticated native bootstrap pair: the
//! serving host's list of the VIEWER's own machines (the same Tailscale user)
//! and of the host itself. See `PROTOCOL_DIRECTORY.md`.
//!
//! Like capability inspection this never selects a session: no approval,
//! observation, input or native worker exists in this mode, and a listing
//! grants nothing. Each listed machine is reached by its own name, under its
//! own certificate and its own host's admission.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    negotiation::{self, Capability, Offer, Role},
    record::{Reader, Writer},
};
use core::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

pub const CAPABILITY: &str = "native-tailnet-directory";
pub const VERSION: u16 = 1;
/// The whole exchange from the startup owner's creation, parked time
/// included: negotiation, one admission refresh, one `LocalAPI` status read
/// and the listing.
pub const MAX_DURATION_US: u64 = 5_000_000;
pub const MAX_ENTRIES: usize = 256;
pub const MAX_RECORDS: usize = 64;
/// All listing records together, headers included.
pub const MAX_LISTING_BYTES: usize = 128 * 1024;
pub const MAX_NAME: usize = 253;
pub const MAX_NODE_ID: usize = 64;
pub const MAX_OS: usize = 32;
pub const REQUEST_BYTES: usize = HEADER_BYTES + 4;
const MAX_LABEL: usize = 63;
const HEAD: usize = 4;
const MAX_ENTRY_BYTES: usize = 4 + MAX_NAME + MAX_NODE_ID + MAX_OS + 4 + 16;
/// The smallest negotiated control record that carries any one entry.
pub const MIN_RECORD: usize = HEADER_BYTES + HEAD + MAX_ENTRY_BYTES;
const FINAL: u8 = 1;
const TRUNCATED: u8 = 2;
const ONLINE: u8 = 1;
const SERVING: u8 = 2;
const HAS_V4: u8 = 4;
const HAS_V6: u8 = 8;

/// Prepare an explicit directory request. Control intent is never downgraded,
/// and a host that does not implement this required marker refuses it rather
/// than opening a session.
pub fn prepare_offer(mut offer: Offer) -> Result<Offer, negotiation::Error> {
    offer.validate()?;
    if offer.role != Role::Observe || crate::inspection::requested(&offer) {
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

/// The host's answer to a directory request: the ordinary intersection plus
/// the required marker, which occupies one of the existing capability slots.
pub fn intersect(host: &Offer, peer: &Offer) -> Result<Offer, negotiation::Error> {
    host.validate()?;
    peer.validate()?;
    if !requested(peer) || crate::inspection::requested(peer) {
        return Err(negotiation::Error::RequiredCapability);
    }
    let mut ordinary = peer.clone();
    ordinary.capabilities.retain(|c| c.name != CAPABILITY);
    let result = host.intersect(&ordinary)?;
    prepare_offer(result)
}

// One exact request form: version 1, reserved zero, no extension.
const REQUEST: [u8; REQUEST_BYTES] = {
    let mut bytes = [0; REQUEST_BYTES];
    bytes[0] = b'F';
    bytes[1] = b'R';
    bytes[2] = b'D';
    bytes[3] = b'0';
    let kind = (Kind::DirectoryRequest as u16).to_be_bytes();
    bytes[6] = kind[0];
    bytes[7] = kind[1];
    bytes[15] = 4;
    let version = VERSION.to_be_bytes();
    bytes[HEADER_BYTES] = version[0];
    bytes[HEADER_BYTES + 1] = version[1];
    bytes
};

/// Viewer-to-host only, once, after `HostCapabilities` carried the marker.
pub fn encode_request(maximum: usize, out: &mut [u8]) -> Result<usize, WireError> {
    if REQUEST_BYTES > maximum.min(negotiation::MAX_RECORD) {
        return Err(WireError::ResourceLimit);
    }
    let out = out
        .get_mut(..REQUEST_BYTES)
        .ok_or(WireError::BufferTooSmall)?;
    out.copy_from_slice(&REQUEST);
    Ok(REQUEST_BYTES)
}
pub fn decode_request(bytes: &[u8], maximum: usize) -> Result<(), WireError> {
    if bytes.len() > maximum.min(negotiation::MAX_RECORD) {
        return Err(WireError::ResourceLimit);
    }
    if bytes != REQUEST {
        return Err(WireError::InvalidValue);
    }
    Ok(())
}

/// One listed machine. Debug output never shows its name, node ID or
/// addresses.
#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    /// `MagicDNS` name, lowercase, without the trailing dot.
    pub name: String,
    /// Tailscale's stable node ID.
    pub node_id: String,
    /// Tailscale's OS label (`macOS`, `linux`, ...); empty when unknown.
    pub os: String,
    /// Tailscale's own reachability flag; the serving host is always online.
    pub online: bool,
    /// The host answering this request (always the first entry).
    pub serving: bool,
    pub ipv4: Option<Ipv4Addr>,
    pub ipv6: Option<Ipv6Addr>,
}
impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("online", &self.online)
            .field("serving", &self.serving)
            .field("ipv4", &self.ipv4.is_some())
            .field("ipv6", &self.ipv6.is_some())
            .finish_non_exhaustive()
    }
}
fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME {
        return false;
    }
    let mut labels = 0;
    for label in name.split('.') {
        let bytes = label.as_bytes();
        labels += 1;
        if bytes.is_empty()
            || bytes.len() > MAX_LABEL
            || bytes[0] == b'-'
            || bytes[bytes.len() - 1] == b'-'
            || !bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        {
            return false;
        }
    }
    labels >= 2
}
fn valid_node_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_NODE_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn valid_os(os: &str) -> bool {
    os.len() <= MAX_OS
        && os
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'-' | b'_'))
}
/// Tailscale assigns node addresses only from `100.64.0.0/10` and
/// `fd7a:115c:a1e0::/48`; nothing else is ever listed.
pub fn tailnet_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}
pub fn tailnet_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
}
impl Entry {
    pub fn validate(&self) -> Result<(), WireError> {
        if !valid_name(&self.name)
            || !valid_node_id(&self.node_id)
            || !valid_os(&self.os)
            || (self.ipv4.is_none() && self.ipv6.is_none())
            || self.ipv4.is_some_and(|ip| !tailnet_v4(ip))
            || self.ipv6.is_some_and(|ip| !tailnet_v6(ip))
            || (self.serving && !self.online)
        {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
    fn size(&self) -> usize {
        4 + self.name.len()
            + self.node_id.len()
            + self.os.len()
            + if self.ipv4.is_some() { 4 } else { 0 }
            + if self.ipv6.is_some() { 16 } else { 0 }
    }
    fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        let mut flags = 0;
        for (set, bit) in [
            (self.online, ONLINE),
            (self.serving, SERVING),
            (self.ipv4.is_some(), HAS_V4),
            (self.ipv6.is_some(), HAS_V6),
        ] {
            if set {
                flags |= bit;
            }
        }
        w.u8(flags)?;
        for text in [&self.name, &self.node_id, &self.os] {
            w.u8(u8::try_from(text.len()).map_err(|_| WireError::InvalidValue)?)?;
            w.put(text.as_bytes())?;
        }
        if let Some(ip) = self.ipv4 {
            w.put(&ip.octets())?;
        }
        if let Some(ip) = self.ipv6 {
            w.put(&ip.octets())?;
        }
        Ok(())
    }
    fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let flags = r.u8()?;
        if flags & !(ONLINE | SERVING | HAS_V4 | HAS_V6) != 0 {
            return Err(WireError::InvalidValue);
        }
        let mut text = || -> Result<String, WireError> {
            let len = usize::from(r.u8()?);
            let bytes = r.take(len)?;
            if !bytes.is_ascii() {
                return Err(WireError::InvalidValue);
            }
            std::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(|_| WireError::InvalidValue)
        };
        let (name, node_id, os) = (text()?, text()?, text()?);
        let ipv4 = if flags & HAS_V4 != 0 {
            let octets: [u8; 4] = r.take(4)?.try_into().map_err(|_| WireError::Truncated)?;
            Some(Ipv4Addr::from(octets))
        } else {
            None
        };
        let ipv6 = if flags & HAS_V6 != 0 {
            let octets: [u8; 16] = r.take(16)?.try_into().map_err(|_| WireError::Truncated)?;
            Some(Ipv6Addr::from(octets))
        } else {
            None
        };
        let entry = Self {
            name,
            node_id,
            os,
            online: flags & ONLINE != 0,
            serving: flags & SERVING != 0,
            ipv4,
            ipv6,
        };
        entry.validate()?;
        Ok(entry)
    }
}

/// A complete listing: the serving host first, then the viewer's machines.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// The host had more matching machines than one listing carries.
    pub truncated: bool,
}
impl fmt::Debug for Listing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listing")
            .field("entries", &self.entries.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}
/// Check `entry` against those already accepted: only the first is the
/// serving host, and names and node IDs are unique.
fn admit(accepted: &[Entry], entry: &Entry) -> Result<(), WireError> {
    if accepted.len() >= MAX_ENTRIES {
        return Err(WireError::ResourceLimit);
    }
    if entry.serving != accepted.is_empty()
        || accepted
            .iter()
            .any(|e| e.name == entry.name || e.node_id == entry.node_id)
    {
        return Err(WireError::InvalidValue);
    }
    Ok(())
}

/// Encode a listing as consecutive host-to-viewer records of at most
/// `maximum` bytes each. Entries beyond the record, byte or entry bounds are
/// dropped and the final record says so (`truncated`).
pub fn encode_listing(listing: &Listing, maximum: usize) -> Result<Vec<Vec<u8>>, WireError> {
    let maximum = maximum.min(negotiation::MAX_RECORD);
    if maximum < MIN_RECORD {
        return Err(WireError::ResourceLimit);
    }
    if listing.entries.is_empty() {
        return Err(WireError::InvalidValue);
    }
    for (i, entry) in listing.entries.iter().enumerate() {
        entry.validate()?;
        if i < MAX_ENTRIES {
            admit(&listing.entries[..i], entry)?;
        }
    }
    let entries = &listing.entries[..listing.entries.len().min(MAX_ENTRIES)];
    let budget = maximum - HEADER_BYTES - HEAD;
    let mut chunks = Vec::new();
    let (mut start, mut total) = (0, 0);
    while start < entries.len() && chunks.len() < MAX_RECORDS {
        let (mut end, mut size) = (start, 0);
        while end < entries.len() && end - start < usize::from(u8::MAX) {
            let next = entries[end].size();
            if size + next > budget {
                break;
            }
            size += next;
            end += 1;
        }
        if end == start {
            return Err(WireError::ResourceLimit);
        }
        let bytes = HEADER_BYTES + HEAD + size;
        if total + bytes > MAX_LISTING_BYTES {
            break;
        }
        total += bytes;
        chunks.push((start, end, size));
        start = end;
    }
    let truncated = listing.truncated || start < listing.entries.len();
    let mut records = Vec::new();
    records
        .try_reserve_exact(chunks.len())
        .map_err(|_| WireError::ResourceLimit)?;
    for (sequence, &(start, end, size)) in chunks.iter().enumerate() {
        let last = sequence + 1 == chunks.len();
        let mut out = vec![0; HEADER_BYTES + HEAD + size];
        let mut w =
            Writer::record_bounded(&mut out, maximum, 0, Kind::DirectoryListing, HEAD + size)?;
        w.u8(u8::try_from(sequence).map_err(|_| WireError::ResourceLimit)?)?;
        w.u8(match (last, truncated) {
            (true, true) => FINAL | TRUNCATED,
            (true, false) => FINAL,
            _ => 0,
        })?;
        w.u8(u8::try_from(end - start).map_err(|_| WireError::ResourceLimit)?)?;
        w.u8(0)?;
        for entry in &entries[start..end] {
            entry.write(&mut w)?;
        }
        let len = w.finish()?;
        out.truncate(len);
        records.push(out);
    }
    Ok(records)
}

struct Part {
    sequence: u8,
    flags: u8,
    entries: Vec<Entry>,
}
fn parse(bytes: &[u8], maximum: usize) -> Result<Part, WireError> {
    // No extension area: the framer and this check refuse one.
    if bytes.get(20..24) != Some(&[0; 4]) {
        return Err(WireError::InvalidExtension);
    }
    let record = Record::decode_bounded(bytes, maximum.min(negotiation::MAX_RECORD), 0, None)?;
    let mut r = record.reader(Kind::DirectoryListing)?;
    let (sequence, flags, count, reserved) = (r.u8()?, r.u8()?, r.u8()?, r.u8()?);
    if usize::from(sequence) >= MAX_RECORDS
        || flags & !(FINAL | TRUNCATED) != 0
        || (flags & TRUNCATED != 0 && flags & FINAL == 0)
        || count == 0
        || reserved != 0
    {
        return Err(WireError::InvalidValue);
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(usize::from(count))
        .map_err(|_| WireError::ResourceLimit)?;
    for _ in 0..count {
        entries.push(Entry::read(&mut r)?);
    }
    r.finish()?;
    Ok(Part {
        sequence,
        flags,
        entries,
    })
}
/// Structural check of one listing record (the transport family's exact
/// validation). Ordering across records is the [`Assembler`]'s.
pub fn check_record(bytes: &[u8], maximum: usize) -> Result<(), WireError> {
    parse(bytes, maximum).map(|_| ())
}

/// The viewer's side: consecutive records into one bounded listing.
#[derive(Debug, Default)]
pub struct Assembler {
    listing: Listing,
    records: usize,
    bytes: usize,
    done: bool,
}
impl Assembler {
    /// Returns true once the final record arrived. Any record after it, a
    /// gap or repeat in sequence, or a broken bound is refused.
    pub fn push(&mut self, bytes: &[u8], maximum: usize) -> Result<bool, WireError> {
        if self.done {
            return Err(WireError::InvalidValue);
        }
        let part = parse(bytes, maximum)?;
        if usize::from(part.sequence) != self.records {
            return Err(WireError::InvalidValue);
        }
        self.records += 1;
        self.bytes += bytes.len();
        if self.bytes > MAX_LISTING_BYTES {
            return Err(WireError::ResourceLimit);
        }
        for entry in part.entries {
            admit(&self.listing.entries, &entry)?;
            self.listing.entries.push(entry);
        }
        if part.flags & FINAL != 0 {
            self.done = true;
            self.listing.truncated = part.flags & TRUNCATED != 0;
        }
        Ok(self.done)
    }
    pub const fn is_complete(&self) -> bool {
        self.done
    }
    pub fn finish(self) -> Result<Listing, WireError> {
        if !self.done {
            return Err(WireError::Truncated);
        }
        Ok(self.listing)
    }
}
