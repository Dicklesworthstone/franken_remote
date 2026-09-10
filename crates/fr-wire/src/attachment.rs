//! Native media channel binding and one-use attachment records. The negotiated
//! native profile uses two actual unidirectional streams per logical channel.
//! These codecs confer no authority and do not allocate or install transport.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, OsSessionId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};

pub const CAPABILITY: &str = "native-media-attachment";
pub const VERSION: u16 = 1;
pub const BINDING_RECORD_BYTES: usize = HEADER_BYTES + 118;
pub const GRANT_RECORD_BYTES: usize = BINDING_RECORD_BYTES + 44;

/// Primary data direction is host-to-viewer. Each channel also has a reliable
/// reverse lane for its role-specific acknowledgements or repair requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaRole {
    Configuration = 1,
    Recovery = 2,
    Video = 3,
}
impl MediaRole {
    fn read(v: u8) -> Result<Self, WireError> {
        match v {
            1 => Ok(Self::Configuration),
            2 => Ok(Self::Recovery),
            3 => Ok(Self::Video),
            _ => Err(WireError::InvalidValue),
        }
    }
}
/// A complete immutable, observation-only media tuple. No absent input lease
/// is represented by a zero-valued lease. Stream IDs are adapter allocations,
/// not hard-coded assignments of a role to a particular QUIC stream number.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub binding: Binding,
    pub role: MediaRole,
    pub host_stream: u64,
    pub viewer_stream: u64,
}
impl fmt::Debug for Descriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaAttachment")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}
impl Descriptor {
    pub fn validate(self, parent: ControlBinding) -> Result<(), WireError> {
        self.binding.validate()?;
        let b = self.binding.parent;
        if parent.id == 0
            || b.id == parent.id
            || b.host_boot != parent.host_boot
            || b.os_session != parent.os_session
            || b.remote_session != parent.remote_session
            || self.host_stream >= (1_u64 << 62)
            || self.viewer_stream >= (1_u64 << 62)
            || self.host_stream & 3 != 3
            || self.viewer_stream & 3 != 2
        {
            return Err(WireError::InvalidBinding);
        }
        Ok(())
    }
}
/// Opaque single-use secret. Its source must be qualified host randomness.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ticket(pub u128);
impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Ticket([redacted])")
    }
}
/// Fixed resource reservation carried identically by ticket, attach and attached.
/// Host time is opaque to the viewer; it cannot reset its own queue deadline.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub descriptor: Descriptor,
    pub ticket: Ticket,
    pub deadline_us: u64,
    pub byte_allowance: u64,
    pub picture_allowance: u32,
    pub credit_epoch: u64,
}
impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelGrant")
            .field("role", &self.descriptor.role)
            .finish_non_exhaustive()
    }
}
impl Grant {
    pub fn validate(
        self,
        parent: ControlBinding,
        limits: &ProtocolLimits,
    ) -> Result<(), WireError> {
        self.descriptor.validate(parent)?;
        if self.ticket.0 == 0
            || self.deadline_us == 0
            || self.credit_epoch == 0
            || self.byte_allowance == 0
            || self.byte_allowance > u64::from(limits.max_control_message_bytes())
            || self.picture_allowance != 0
        {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}
/// `picture_allowance` is zero for this channel-attachment profile: configuration,
/// recovery and progress records are bounded byte records. Encoded-picture and
/// datagram admission additionally requires the existing media-owner budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    Binding(Descriptor),
    Accepted(u32),
    Ticket(Grant),
    Attach(Grant),
    Attached(Grant),
}
impl Message {
    fn kind(self) -> Kind {
        match self {
            Self::Binding(_) => Kind::StreamBinding,
            Self::Accepted(_) => Kind::BindingAccepted,
            Self::Ticket(_) => Kind::ChannelTicket,
            Self::Attach(_) => Kind::ChannelAttach,
            Self::Attached(_) => Kind::ChannelAttached,
        }
    }
    pub const fn descriptor(self) -> Option<Descriptor> {
        match self {
            Self::Binding(d) => Some(d),
            Self::Ticket(g) | Self::Attach(g) | Self::Attached(g) => Some(g.descriptor),
            Self::Accepted(_) => None,
        }
    }
}
fn direction(
    kind: Kind,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    let expected = if matches!(kind, Kind::ChannelAttach | Kind::BindingAccepted) {
        InputDirection::ViewerToHost
    } else {
        InputDirection::HostToViewer
    };
    if direction != expected {
        return Err(WireError::WrongRole);
    }
    Ok(())
}
fn write_descriptor(w: &mut Writer<'_>, d: Descriptor) -> Result<(), WireError> {
    let b = d.binding;
    w.u32(b.parent.id)?;
    for n in [
        b.parent.host_boot.as_raw(),
        b.parent.os_session.as_raw(),
        b.parent.remote_session.as_raw(),
        b.display,
    ] {
        w.put(&n.to_be_bytes())?;
    }
    for n in [
        b.geometry.as_raw(),
        b.configuration.as_raw(),
        b.recovery.as_raw(),
        b.viewport.as_raw(),
    ] {
        w.u64(n)?;
    }
    w.u8(d.role as u8)?;
    w.u8(1)?; // The primary media direction is always host -> viewer.
    w.u64(d.host_stream)?;
    w.u64(d.viewer_stream)
}
fn u128_value(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}
fn read_descriptor(r: &mut Reader<'_>) -> Result<Descriptor, WireError> {
    let binding = Binding {
        parent: ControlBinding {
            id: r.u32()?,
            host_boot: HostBootId::from_raw(u128_value(r)?),
            os_session: OsSessionId::from_raw(u128_value(r)?),
            remote_session: RemoteSessionId::from_raw(u128_value(r)?),
        },
        display: u128_value(r)?,
        geometry: DisplayGeometryGeneration::from_raw(r.u64()?),
        configuration: CodecConfigurationGeneration::from_raw(r.u64()?),
        recovery: RecoveryGeneration::from_raw(r.u64()?),
        viewport: ViewportMappingGeneration::from_raw(r.u64()?),
    };
    let role = MediaRole::read(r.u8()?)?;
    if r.u8()? != 1 {
        return Err(WireError::WrongRole);
    }
    Ok(Descriptor {
        binding,
        role,
        host_stream: r.u64()?,
        viewer_stream: r.u64()?,
    })
}
pub fn encode(
    message: Message,
    parent: ControlBinding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    sender: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    let kind = message.kind();
    direction(kind, sender, delivery)?;
    if let Message::Accepted(binding) = message {
        if binding == 0 || parent.id == 0 || binding == parent.id {
            return Err(WireError::InvalidBinding);
        }
        let mut w = Writer::record_bounded(
            out,
            limits.max_control_message_bytes() as usize,
            parent.id,
            kind,
            4,
        )?;
        w.u32(binding)?;
        return w.finish();
    }
    let descriptor = message.descriptor().ok_or(WireError::InvalidValue)?;
    descriptor.validate(parent)?;
    let (binding, total) = match message {
        Message::Accepted(_) => return Err(WireError::InvalidValue),
        Message::Binding(_) => (parent.id, BINDING_RECORD_BYTES),
        Message::Ticket(g) => {
            g.validate(parent, limits)?;
            (parent.id, GRANT_RECORD_BYTES)
        }
        Message::Attach(g) | Message::Attached(g) => {
            g.validate(parent, limits)?;
            (descriptor.binding.parent.id, GRANT_RECORD_BYTES)
        }
    };
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding,
        kind,
        total - HEADER_BYTES,
    )?;
    write_descriptor(&mut w, descriptor)?;
    if let Message::Ticket(g) | Message::Attach(g) | Message::Attached(g) = message {
        w.put(&g.ticket.0.to_be_bytes())?;
        w.u64(g.deadline_us)?;
        w.u64(g.byte_allowance)?;
        w.u32(g.picture_allowance)?;
        w.u64(g.credit_epoch)?;
    }
    w.finish()
}
pub fn decode(
    bytes: &[u8],
    parent: ControlBinding,
    channel_binding: u32,
    limits: &ProtocolLimits,
    sender: InputDirection,
    delivery: InputDelivery,
) -> Result<Message, WireError> {
    let record = Record::decode_bounded(
        bytes,
        (limits.max_control_message_bytes() as usize).min(GRANT_RECORD_BYTES),
        channel_binding,
        None,
    )?;
    let kind = record.kind();
    if !matches!(
        kind,
        Kind::StreamBinding
            | Kind::BindingAccepted
            | Kind::ChannelTicket
            | Kind::ChannelAttach
            | Kind::ChannelAttached
    ) {
        return Err(WireError::UnsupportedKind);
    }
    direction(kind, sender, delivery)?;
    let mut r = record.reader(kind)?;
    if kind == Kind::BindingAccepted {
        let binding = r.u32()?;
        r.finish()?;
        if binding == 0 || binding == parent.id || channel_binding != parent.id {
            return Err(WireError::InvalidBinding);
        }
        return Ok(Message::Accepted(binding));
    }
    let d = read_descriptor(&mut r)?;
    d.validate(parent)?;
    let expected = if matches!(kind, Kind::StreamBinding | Kind::ChannelTicket) {
        parent.id
    } else {
        d.binding.parent.id
    };
    if channel_binding != expected {
        return Err(WireError::InvalidBinding);
    }
    let message = if kind == Kind::StreamBinding {
        Message::Binding(d)
    } else {
        let g = Grant {
            descriptor: d,
            ticket: Ticket(u128_value(&mut r)?),
            deadline_us: r.u64()?,
            byte_allowance: r.u64()?,
            picture_allowance: r.u32()?,
            credit_epoch: r.u64()?,
        };
        g.validate(parent, limits)?;
        match kind {
            Kind::ChannelTicket => Message::Ticket(g),
            Kind::ChannelAttach => Message::Attach(g),
            _ => Message::Attached(g),
        }
    };
    r.finish()?;
    Ok(message)
}
