//! Bounded native-session startup, not peer identity, approval, or codec probing.
//! See `PROTOCOL_NEGOTIATION.md` for exact v0 bytes. No borrowed record escapes
//! its transport turn; owned collections have small independent count bounds.
use crate::{
    Kind, Record, WireError,
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::{LimitOverrides, ProtocolLimits},
};

pub const MAX_RECORD: usize = 4096;
pub const MAX_CAPABILITIES: usize = 16;
pub const MAX_VERSIONS: usize = 8;
const MAX_NAME: usize = 64;
const LIMIT_BYTES: usize = 33;
/// The native profile version is independent of the application version.
pub const NATIVE_PROFILE: u8 = 1;
pub const PROFILE_VERSION: u16 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Observe,
    RequestControl,
}
impl Role {
    const fn byte(self) -> u8 {
        match self {
            Self::Observe => 0,
            Self::RequestControl => 1,
        }
    }
    fn read(r: &mut Reader<'_>) -> Result<Self, Error> {
        match r.u8()? {
            0 => Ok(Self::Observe),
            1 => Ok(Self::RequestControl),
            _ => Err(Error::Invalid),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Wire(WireError),
    Invalid,
    Limits,
    Version,
    Profile,
    RequiredCapability,
    Selection,
    Allocation,
}
impl From<WireError> for Error {
    fn from(e: WireError) -> Self {
        Self::Wire(e)
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for Error {}

#[derive(Clone, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub version: u16,
    pub required: bool,
}
impl fmt::Debug for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capability")
            .field("version", &self.version)
            .field("required", &self.required)
            .finish_non_exhaustive()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub versions: Vec<u16>,
    pub profile: u8,
    pub profile_version: u16,
    pub role: Role,
    pub limits: ProtocolLimits,
    pub capabilities: Vec<Capability>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub version: u16,
    pub profile: u8,
    pub profile_version: u16,
    pub role: Role,
    pub limits: ProtocolLimits,
    pub capabilities: Vec<Capability>,
}
/// Only the initial connection-control tuple. Display, geometry, codec,
/// recovery, viewport and input lease are explicitly absent, not zero aliases.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ControlBinding {
    pub id: u32,
    pub host_boot: HostBootId,
    pub os_session: OsSessionId,
    pub remote_session: RemoteSessionId,
}
impl fmt::Debug for ControlBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ControlBinding([redacted])")
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    ClientHello(Offer),
    HostCapabilities(Offer),
    SelectedConfiguration(Selection),
    ApprovalRequired {
        request: RemoteSessionId,
        deadline_us: u64,
        role: Role,
    },
    SessionOpened {
        binding: ControlBinding,
        selection: Selection,
        observation_until_us: u64,
    },
    BindingAccepted {
        binding: u32,
    },
}
impl Message {
    pub const fn kind(&self) -> Kind {
        match self {
            Self::ClientHello(_) => Kind::ClientHello,
            Self::HostCapabilities(_) => Kind::HostCapabilities,
            Self::SelectedConfiguration(_) => Kind::SelectedConfiguration,
            Self::ApprovalRequired { .. } => Kind::ApprovalRequired,
            Self::SessionOpened { .. } => Kind::SessionOpened,
            Self::BindingAccepted { .. } => Kind::BindingAccepted,
        }
    }
    pub const fn binding(&self) -> u32 {
        match self {
            Self::BindingAccepted { binding } => *binding,
            _ => 0,
        }
    }
}
fn valid_caps(caps: &[Capability]) -> Result<(), Error> {
    if caps.len() > MAX_CAPABILITIES {
        return Err(Error::Invalid);
    }
    let mut previous: Option<&str> = None;
    for c in caps {
        if c.name.is_empty()
            || c.name.len() > MAX_NAME
            || c.version == 0
            || !c
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-_".contains(&b))
            || previous.is_some_and(|p| p >= c.name.as_str())
        {
            return Err(Error::Invalid);
        }
        previous = Some(&c.name);
    }
    Ok(())
}
fn valid_limits(limits: ProtocolLimits) -> Result<(), Error> {
    // The fixed startup offer itself must fit even with no capabilities.
    if limits.max_control_message_bytes() < 128 {
        return Err(Error::Limits);
    }
    Ok(())
}
impl Offer {
    pub fn validate(&self) -> Result<(), Error> {
        if self.versions.is_empty()
            || self.versions.len() > MAX_VERSIONS
            || self.versions.windows(2).any(|w| w[0] >= w[1])
        {
            return Err(Error::Version);
        }
        if self.profile != NATIVE_PROFILE || self.profile_version != PROFILE_VERSION {
            return Err(Error::Profile);
        }
        valid_limits(self.limits)?;
        valid_caps(&self.capabilities)
    }
    /// The host's implemented intersection. Neither side can erase the other's
    /// required capability, and an unknown future app version is never selected.
    pub fn intersect(&self, peer: &Self) -> Result<Self, Error> {
        self.validate()?;
        peer.validate()?;
        if !self.versions.contains(&0) || !peer.versions.contains(&0) {
            return Err(Error::Version);
        }
        for (a, b) in [
            (&self.capabilities, &peer.capabilities),
            (&peer.capabilities, &self.capabilities),
        ] {
            if a.iter().any(|c| {
                c.required && !b.iter().any(|p| p.name == c.name && p.version == c.version)
            }) {
                return Err(Error::RequiredCapability);
            }
        }
        let mut capabilities = Vec::new();
        capabilities
            .try_reserve_exact(MAX_CAPABILITIES)
            .map_err(|_| Error::Allocation)?;
        for c in &self.capabilities {
            if let Some(p) = peer
                .capabilities
                .iter()
                .find(|p| p.name == c.name && p.version == c.version)
            {
                capabilities.push(Capability {
                    required: c.required || p.required,
                    ..c.clone()
                });
            }
        }
        Ok(Self {
            versions: vec![0],
            role: peer.role,
            limits: self.limits.negotiated(&peer.limits),
            capabilities,
            ..self.clone()
        })
    }
    pub fn select(&self) -> Result<Selection, Error> {
        self.validate()?;
        if !self.versions.contains(&0) {
            return Err(Error::Version);
        }
        Ok(Selection {
            version: 0,
            profile: self.profile,
            profile_version: self.profile_version,
            role: self.role,
            limits: self.limits,
            capabilities: self.capabilities.clone(),
        })
    }
    /// Verify a received host intersection against the original client offer.
    pub fn check_host(&self, host: &Self) -> Result<(), Error> {
        self.validate()?;
        host.validate()?;
        if host.versions != [0]
            || !self.versions.contains(&0)
            || host.role != self.role
            || host.limits.negotiated(&self.limits) != host.limits
        {
            return Err(Error::Selection);
        }
        if host.capabilities.iter().any(|c| {
            !self
                .capabilities
                .iter()
                .any(|p| p.name == c.name && p.version == c.version)
        }) || self.capabilities.iter().any(|c| {
            c.required
                && !host
                    .capabilities
                    .iter()
                    .any(|p| p.name == c.name && p.version == c.version && p.required)
        }) {
            return Err(Error::RequiredCapability);
        }
        Ok(())
    }
}
impl Selection {
    pub fn validate(&self) -> Result<(), Error> {
        if self.version != 0 {
            return Err(Error::Version);
        }
        if self.profile != NATIVE_PROFILE || self.profile_version != PROFILE_VERSION {
            return Err(Error::Profile);
        }
        valid_limits(self.limits)?;
        valid_caps(&self.capabilities)
    }
    pub fn check_against(&self, offer: &Offer) -> Result<(), Error> {
        self.validate()?;
        offer.validate()?;
        if !offer.versions.contains(&self.version)
            || self.role != offer.role
            || self.limits.negotiated(&offer.limits) != self.limits
        {
            return Err(Error::Selection);
        }
        if self
            .capabilities
            .iter()
            .any(|c| !offer.capabilities.iter().any(|p| p == c))
            || offer
                .capabilities
                .iter()
                .any(|c| c.required && !self.capabilities.contains(c))
        {
            return Err(Error::RequiredCapability);
        }
        Ok(())
    }
}
fn read_caps(r: &mut Reader<'_>) -> Result<Vec<Capability>, Error> {
    let n = usize::try_from(r.u32()?).map_err(|_| Error::Invalid)?;
    if n > MAX_CAPABILITIES || n > r.remaining.len() / 8 {
        return Err(Error::Invalid);
    }
    let mut result = Vec::new();
    result.try_reserve_exact(n).map_err(|_| Error::Allocation)?;
    for _ in 0..n {
        let bytes = r.data()?;
        if bytes.is_empty() || bytes.len() > MAX_NAME {
            return Err(Error::Invalid);
        }
        let name = core::str::from_utf8(bytes)
            .map_err(|_| Error::Invalid)?
            .to_owned();
        let version = r.u16()?;
        let required = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Error::Invalid),
        };
        result.push(Capability {
            name,
            version,
            required,
        });
    }
    valid_caps(&result)?;
    Ok(result)
}
fn caps_bytes(caps: &[Capability]) -> usize {
    4 + caps.iter().map(|c| 7 + c.name.len()).sum::<usize>()
}
fn write_caps(w: &mut Writer<'_>, caps: &[Capability]) -> Result<(), Error> {
    w.u32(u32::try_from(caps.len()).map_err(|_| Error::Invalid)?)?;
    for c in caps {
        w.data(c.name.as_bytes())?;
        w.u16(c.version)?;
        w.u8(u8::from(c.required))?;
    }
    Ok(())
}
fn read_limits(r: &mut Reader<'_>) -> Result<ProtocolLimits, Error> {
    ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(r.u32()?),
        max_clipboard_item_bytes: Some(r.u32()?),
        max_encoded_access_unit_bytes: Some(r.u32()?),
        max_dimension_pixels: Some(r.u32()?),
        max_coded_pixels: Some(r.u64()?),
        reassembly_window_pictures: Some(r.u8()?),
        per_viewer_compressed_bytes: Some(r.u64()?),
    })
    .map_err(|_| Error::Limits)
}
fn write_limits(w: &mut Writer<'_>, l: ProtocolLimits) -> Result<(), Error> {
    w.u32(l.max_control_message_bytes())?;
    w.u32(l.max_clipboard_item_bytes())?;
    w.u32(l.max_encoded_access_unit_bytes())?;
    w.u32(l.max_dimension_pixels())?;
    w.u64(l.max_coded_pixels())?;
    w.u8(l.reassembly_window_pictures())?;
    w.u64(l.per_viewer_compressed_bytes())?;
    Ok(())
}
fn offer_bytes(o: &Offer) -> usize {
    4 + o.versions.len() * 2 + 4 + LIMIT_BYTES + caps_bytes(&o.capabilities) + 1
}
fn selection_bytes(s: &Selection) -> usize {
    6 + LIMIT_BYTES + caps_bytes(&s.capabilities)
}
fn write_offer(w: &mut Writer<'_>, o: &Offer) -> Result<(), Error> {
    w.u32(u32::try_from(o.versions.len()).map_err(|_| Error::Version)?)?;
    for &v in &o.versions {
        w.u16(v)?;
    }
    w.u8(o.profile)?;
    w.u16(o.profile_version)?;
    w.u8(o.role.byte())?;
    write_limits(w, o.limits)?;
    write_caps(w, &o.capabilities)?;
    // Native QUIC has no Origin bootstrap nonce. Browser profiles are refused.
    w.u8(0)?;
    Ok(())
}
fn read_offer(r: &mut Reader<'_>) -> Result<Offer, Error> {
    let n = usize::try_from(r.u32()?).map_err(|_| Error::Version)?;
    if n == 0 || n > MAX_VERSIONS || n > r.remaining.len() / 2 {
        return Err(Error::Version);
    }
    let mut versions = Vec::new();
    versions
        .try_reserve_exact(n)
        .map_err(|_| Error::Allocation)?;
    for _ in 0..n {
        versions.push(r.u16()?);
    }
    let offer = Offer {
        versions,
        profile: r.u8()?,
        profile_version: r.u16()?,
        role: Role::read(r)?,
        limits: read_limits(r)?,
        capabilities: read_caps(r)?,
    };
    if r.u8()? != 0 {
        return Err(Error::Profile);
    }
    offer.validate()?;
    Ok(offer)
}
fn write_selection(w: &mut Writer<'_>, s: &Selection) -> Result<(), Error> {
    w.u16(s.version)?;
    w.u8(s.profile)?;
    w.u16(s.profile_version)?;
    w.u8(s.role.byte())?;
    write_limits(w, s.limits)?;
    write_caps(w, &s.capabilities)
}
fn read_selection(r: &mut Reader<'_>) -> Result<Selection, Error> {
    let s = Selection {
        version: r.u16()?,
        profile: r.u8()?,
        profile_version: r.u16()?,
        role: Role::read(r)?,
        limits: read_limits(r)?,
        capabilities: read_caps(r)?,
    };
    s.validate()?;
    Ok(s)
}
pub fn encode(message: &Message, maximum: usize, out: &mut [u8]) -> Result<usize, Error> {
    let size = match message {
        Message::ClientHello(o) | Message::HostCapabilities(o) => {
            o.validate()?;
            offer_bytes(o)
        }
        Message::SelectedConfiguration(s) => {
            s.validate()?;
            selection_bytes(s)
        }
        Message::ApprovalRequired { deadline_us, .. } => {
            if *deadline_us == 0 {
                return Err(Error::Invalid);
            }
            25
        }
        Message::SessionOpened {
            binding,
            selection,
            observation_until_us,
        } => {
            if binding.id == 0 || *observation_until_us == 0 {
                return Err(Error::Invalid);
            }
            selection.validate()?;
            68 + selection_bytes(selection)
        }
        Message::BindingAccepted { binding } => {
            if *binding == 0 {
                return Err(Error::Invalid);
            }
            4
        }
    };
    let mut w = Writer::record_bounded(
        out,
        maximum.min(MAX_RECORD),
        message.binding(),
        message.kind(),
        size,
    )?;
    match message {
        Message::ClientHello(o) | Message::HostCapabilities(o) => write_offer(&mut w, o)?,
        Message::SelectedConfiguration(s) => write_selection(&mut w, s)?,
        Message::ApprovalRequired {
            request,
            deadline_us,
            role,
        } => {
            w.put(&request.as_raw().to_be_bytes())?;
            w.u64(*deadline_us)?;
            w.u8(role.byte())?;
        }
        Message::SessionOpened {
            binding,
            selection,
            observation_until_us,
        } => {
            w.put(&binding.host_boot.as_raw().to_be_bytes())?;
            w.put(&binding.os_session.as_raw().to_be_bytes())?;
            w.put(&binding.remote_session.as_raw().to_be_bytes())?;
            w.u32(binding.id)?;
            w.put(&[0; 6])?;
            w.u8(1)?;
            w.u8(0)?;
            write_selection(&mut w, selection)?;
            w.u64(*observation_until_us)?;
        }
        Message::BindingAccepted { binding } => w.u32(*binding)?,
    }
    Ok(w.finish()?)
}
pub fn decode(bytes: &[u8], maximum: usize, expected_binding: u32) -> Result<Message, Error> {
    let record = Record::decode_bounded(bytes, maximum.min(MAX_RECORD), expected_binding, None)?;
    let kind = record.kind();
    // Bootstrap kinds must use zero; only BindingAccepted uses its installed ID.
    if kind.initial() && expected_binding != 0 {
        return Err(Error::Invalid);
    }
    let mut r = record.reader(kind)?;
    let id = |r: &mut Reader<'_>| -> Result<u128, Error> {
        Ok(u128::from_be_bytes(
            r.take(16)?.try_into().map_err(|_| Error::Invalid)?,
        ))
    };
    let message = match kind {
        Kind::ClientHello => Message::ClientHello(read_offer(&mut r)?),
        Kind::HostCapabilities => Message::HostCapabilities(read_offer(&mut r)?),
        Kind::SelectedConfiguration => Message::SelectedConfiguration(read_selection(&mut r)?),
        Kind::ApprovalRequired => {
            let request = RemoteSessionId::from_raw(id(&mut r)?);
            let deadline_us = r.u64()?;
            if deadline_us == 0 {
                return Err(Error::Invalid);
            }
            Message::ApprovalRequired {
                request,
                deadline_us,
                role: Role::read(&mut r)?,
            }
        }
        Kind::SessionOpened => {
            let host_boot = HostBootId::from_raw(id(&mut r)?);
            let os_session = OsSessionId::from_raw(id(&mut r)?);
            let remote_session = RemoteSessionId::from_raw(id(&mut r)?);
            let binding_id = r.u32()?;
            if binding_id == 0 || r.take(8)? != [0, 0, 0, 0, 0, 0, 1, 0] {
                return Err(Error::Invalid);
            }
            let selection = read_selection(&mut r)?;
            let observation_until_us = r.u64()?;
            if observation_until_us == 0 {
                return Err(Error::Invalid);
            }
            Message::SessionOpened {
                binding: ControlBinding {
                    id: binding_id,
                    host_boot,
                    os_session,
                    remote_session,
                },
                selection,
                observation_until_us,
            }
        }
        Kind::BindingAccepted => {
            let binding = r.u32()?;
            if binding != expected_binding {
                return Err(Error::Invalid);
            }
            Message::BindingAccepted { binding }
        }
        _ => return Err(Error::Wire(WireError::UnsupportedKind)),
    };
    r.finish()?;
    Ok(message)
}
