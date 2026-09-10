//! Allocation-free record framing. See `PROTOCOL_MEDIA.md` for byte fixtures.
use core::fmt;
use fr_core::limits::ProtocolLimits;

/// Size of the fixed FRD0 record header.
pub const HEADER_BYTES: usize = 24;
/// Absolute implementation ceiling for fragments in one picture.
pub const MAX_FRAGMENTS: u32 = 16_384;
/// Absolute implementation ceiling for missing ranges in one request.
pub const MAX_REPAIR_RANGES: u16 = 64;

/// A bounded refusal containing no peer-controlled text or media content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WireError {
    Truncated,
    TrailingBytes,
    BadMagic,
    UnsupportedVersion,
    UnsupportedKind,
    InvalidFlags,
    InvalidBinding,
    WrongChannel,
    WrongRole,
    InvalidLimits,
    ResourceLimit,
    ArithmeticOverflow,
    InvalidValue,
    InvalidFragment,
    InvalidDependency,
    InvalidExtension,
    RequiredExtension,
    InvalidRanges,
    BufferTooSmall,
}
impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for WireError {}

/// Actual selected caps; constructing an impossible transport profile refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaLimits {
    protocol: ProtocolLimits,
    record_bytes: usize,
    fragments: u32,
    repair_ranges: u16,
}
impl MediaLimits {
    pub fn new(
        protocol: ProtocolLimits,
        record_bytes: usize,
        fragments: u32,
        repair_ranges: u16,
    ) -> Result<Self, WireError> {
        if record_bytes <= super::FRAGMENT_OVERHEAD
            || record_bytes > protocol.max_control_message_bytes() as usize
            || fragments == 0
            || fragments > MAX_FRAGMENTS
            || repair_ranges == 0
            || repair_ranges > MAX_REPAIR_RANGES
        {
            return Err(WireError::InvalidLimits);
        }
        Ok(Self {
            protocol,
            record_bytes,
            fragments,
            repair_ranges,
        })
    }
    pub const fn protocol(&self) -> &ProtocolLimits {
        &self.protocol
    }
    pub const fn record_bytes(&self) -> usize {
        self.record_bytes
    }
    pub const fn max_fragments(&self) -> u32 {
        self.fragments
    }
    pub const fn max_repair_ranges(&self) -> u16 {
        self.repair_ranges
    }
    pub fn fragment_stride(&self) -> u32 {
        u32::try_from(self.record_bytes - super::FRAGMENT_OVERHEAD)
            .expect("bounded by u32 protocol cap")
    }
}

/// Implemented v0 media and input kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    ClientHello = 0x0001,
    HostCapabilities = 0x0002,
    SelectedConfiguration = 0x0003,
    ApprovalRequired = 0x0010,
    SessionOpened = 0x0011,
    Challenge = 0x0015,
    ChallengeResponse = 0x0016,
    InputTicket = 0x0017,
    BindingAccepted = 0x001c,
    DecoderConfiguration = 0x0030,
    DecoderConfigured = 0x0031,
    Recovery = 0x0032,
    FirstFrameDecoded = 0x0033,
    Fragment = 0x0034,
    Repair = 0x0035,
    Progress = 0x0037,
    Key = 0x0040,
    Button = 0x0041,
    Pointer = 0x0042,
    Relative = 0x0043,
    Scroll = 0x0044,
    Text = 0x0045,
    HeldState = 0x0046,
    InputMode = 0x0047,
    InputResult = 0x0048,
    ClockProbe = 0x0084,
    ClockReply = 0x0085,
}
impl Kind {
    pub(crate) const fn initial(self) -> bool {
        matches!(
            self,
            Self::ClientHello
                | Self::HostCapabilities
                | Self::SelectedConfiguration
                | Self::ApprovalRequired
                | Self::SessionOpened
        )
    }
    fn parse(value: u16) -> Result<Self, WireError> {
        match value {
            0x0001 => Ok(Self::ClientHello),
            0x0002 => Ok(Self::HostCapabilities),
            0x0003 => Ok(Self::SelectedConfiguration),
            0x0010 => Ok(Self::ApprovalRequired),
            0x0011 => Ok(Self::SessionOpened),
            0x0015 => Ok(Self::Challenge),
            0x0016 => Ok(Self::ChallengeResponse),
            0x0017 => Ok(Self::InputTicket),
            0x001c => Ok(Self::BindingAccepted),
            0x0030 => Ok(Self::DecoderConfiguration),
            0x0031 => Ok(Self::DecoderConfigured),
            0x0032 => Ok(Self::Recovery),
            0x0033 => Ok(Self::FirstFrameDecoded),
            0x0034 => Ok(Self::Fragment),
            0x0035 => Ok(Self::Repair),
            0x0037 => Ok(Self::Progress),
            0x0040 => Ok(Self::Key),
            0x0041 => Ok(Self::Button),
            0x0042 => Ok(Self::Pointer),
            0x0043 => Ok(Self::Relative),
            0x0044 => Ok(Self::Scroll),
            0x0045 => Ok(Self::Text),
            0x0046 => Ok(Self::HeldState),
            0x0047 => Ok(Self::InputMode),
            0x0048 => Ok(Self::InputResult),
            0x0084 => Ok(Self::ClockProbe),
            0x0085 => Ok(Self::ClockReply),
            _ => Err(WireError::UnsupportedKind),
        }
    }
    const fn channel(self) -> Option<Channel> {
        match self {
            Self::Recovery => Some(Channel::Recovery),
            Self::Fragment => Some(Channel::Video),
            Self::Repair => Some(Channel::Control),
            Self::Progress
            | Self::DecoderConfiguration
            | Self::DecoderConfigured
            | Self::FirstFrameDecoded => Some(Channel::MediaConfig),
            Self::ClientHello
            | Self::HostCapabilities
            | Self::SelectedConfiguration
            | Self::ApprovalRequired
            | Self::SessionOpened
            | Self::Challenge
            | Self::ChallengeResponse
            | Self::InputTicket
            | Self::BindingAccepted
            | Self::Key
            | Self::Button
            | Self::Pointer
            | Self::Relative
            | Self::Scroll
            | Self::HeldState
            | Self::Text
            | Self::InputMode
            | Self::InputResult
            | Self::ClockProbe
            | Self::ClockReply => None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Video,
    Recovery,
    Control,
    MediaConfig,
}

/// A borrowed record. Debug intentionally excludes its payload and extensions.
#[derive(Clone, Copy)]
pub struct Record<'a> {
    kind: Kind,
    binding: u32,
    payload: &'a [u8],
}
impl fmt::Debug for Record<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("kind", &self.kind)
            .field("binding", &self.binding)
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}
impl<'a> Record<'a> {
    /// Parse exactly one datagram/WSS/reliable record for an already admitted
    /// channel binding. A mismatch is rejected before any payload allocation.
    pub fn decode(
        bytes: &'a [u8],
        limits: &MediaLimits,
        binding: u32,
        channel: Channel,
    ) -> Result<Self, WireError> {
        Self::decode_bounded(bytes, limits.record_bytes, binding, Some(channel))
    }
    pub(crate) fn decode_bounded(
        bytes: &'a [u8],
        maximum: usize,
        binding: u32,
        channel: Option<Channel>,
    ) -> Result<Self, WireError> {
        if bytes.len() > maximum {
            return Err(WireError::ResourceLimit);
        }
        let mut r = Reader::new(bytes);
        if r.take(4)? != b"FRD0" {
            return Err(WireError::BadMagic);
        }
        if r.u16()? != 0 {
            return Err(WireError::UnsupportedVersion);
        }
        let kind = Kind::parse(r.u16()?)?;
        if r.u16()? != 0 || r.u16()? != 0 {
            return Err(WireError::InvalidFlags);
        }
        let payload_len = usize::try_from(r.u32()?).map_err(|_| WireError::ArithmeticOverflow)?;
        let received_binding = r.u32()?;
        let ext_len = usize::try_from(r.u32()?).map_err(|_| WireError::ArithmeticOverflow)?;
        if (binding == 0 && !kind.initial()) || received_binding != binding {
            return Err(WireError::InvalidBinding);
        }
        if kind.channel() != channel {
            return Err(WireError::WrongChannel);
        }
        if ext_len > payload_len {
            return Err(WireError::InvalidExtension);
        }
        let payload = r.take(payload_len)?;
        r.finish()?;
        let fixed_len = payload_len - ext_len;
        validate_extensions(&payload[fixed_len..])?;
        Ok(Self {
            kind,
            binding,
            payload: &payload[..fixed_len],
        })
    }
    pub const fn kind(&self) -> Kind {
        self.kind
    }
    pub const fn binding(&self) -> u32 {
        self.binding
    }
    pub(crate) fn reader(&self, expected: Kind) -> Result<Reader<'a>, WireError> {
        if self.kind != expected {
            return Err(WireError::UnsupportedKind);
        }
        Ok(Reader::new(self.payload))
    }
}

fn validate_extensions(bytes: &[u8]) -> Result<(), WireError> {
    let mut r = Reader::new(bytes);
    let mut previous = None;
    let mut count = 0_u16;
    while !r.remaining.is_empty() {
        count += 1;
        if count > 128 {
            return Err(WireError::ResourceLimit);
        }
        let tag = r.u16()?;
        let flags = r.u16()?;
        let len = usize::try_from(r.u32()?).map_err(|_| WireError::ArithmeticOverflow)?;
        if previous.is_some_and(|p| tag <= p) || flags & !1 != 0 {
            return Err(WireError::InvalidExtension);
        }
        r.take(len)?;
        if flags & 1 != 0 {
            return Err(WireError::RequiredExtension);
        }
        previous = Some(tag);
    }
    Ok(())
}

pub(crate) struct Reader<'a> {
    pub(crate) remaining: &'a [u8],
}
impl<'a> Reader<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }
    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let bytes = self.remaining.get(..len).ok_or(WireError::Truncated)?;
        self.remaining = &self.remaining[len..];
        Ok(bytes)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| WireError::Truncated)?,
        ))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| WireError::Truncated)?,
        ))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| WireError::Truncated)?,
        ))
    }
    pub(crate) fn optional_u64(&mut self) -> Result<Option<u64>, WireError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            _ => Err(WireError::InvalidValue),
        }
    }
    pub(crate) fn data(&mut self) -> Result<&'a [u8], WireError> {
        let len = usize::try_from(self.u32()?).map_err(|_| WireError::ArithmeticOverflow)?;
        self.take(len)
    }
    pub(crate) fn finish(self) -> Result<(), WireError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(WireError::TrailingBytes)
        }
    }
}

pub(crate) struct Writer<'a> {
    bytes: &'a mut [u8],
    position: usize,
}
impl<'a> Writer<'a> {
    pub(crate) fn record(
        out: &'a mut [u8],
        limits: &MediaLimits,
        binding: u32,
        kind: Kind,
        payload: usize,
    ) -> Result<Self, WireError> {
        Self::record_bounded(out, limits.record_bytes, binding, kind, payload)
    }
    pub(crate) fn record_bounded(
        out: &'a mut [u8],
        maximum: usize,
        binding: u32,
        kind: Kind,
        payload: usize,
    ) -> Result<Self, WireError> {
        let total = HEADER_BYTES
            .checked_add(payload)
            .ok_or(WireError::ArithmeticOverflow)?;
        if binding == 0 && !kind.initial() {
            return Err(WireError::InvalidBinding);
        }
        if total > maximum {
            return Err(WireError::ResourceLimit);
        }
        if total > out.len() {
            return Err(WireError::BufferTooSmall);
        }
        let mut w = Self {
            bytes: &mut out[..total],
            position: 0,
        };
        w.put(b"FRD0")?;
        w.u16(0)?;
        w.u16(kind as u16)?;
        w.u16(0)?;
        w.u16(0)?;
        w.u32(u32::try_from(payload).map_err(|_| WireError::ArithmeticOverflow)?)?;
        w.u32(binding)?;
        w.u32(0)?;
        Ok(w)
    }
    pub(crate) fn put(&mut self, data: &[u8]) -> Result<(), WireError> {
        let end = self
            .position
            .checked_add(data.len())
            .ok_or(WireError::ArithmeticOverflow)?;
        self.bytes
            .get_mut(self.position..end)
            .ok_or(WireError::BufferTooSmall)?
            .copy_from_slice(data);
        self.position = end;
        Ok(())
    }
    pub(crate) fn u8(&mut self, v: u8) -> Result<(), WireError> {
        self.put(&[v])
    }
    pub(crate) fn u16(&mut self, v: u16) -> Result<(), WireError> {
        self.put(&v.to_be_bytes())
    }
    pub(crate) fn u32(&mut self, v: u32) -> Result<(), WireError> {
        self.put(&v.to_be_bytes())
    }
    pub(crate) fn u64(&mut self, v: u64) -> Result<(), WireError> {
        self.put(&v.to_be_bytes())
    }
    pub(crate) fn optional_u64(&mut self, v: Option<u64>) -> Result<(), WireError> {
        self.u8(u8::from(v.is_some()))?;
        if let Some(value) = v {
            self.u64(value)?;
        }
        Ok(())
    }
    pub(crate) fn data(&mut self, data: &[u8]) -> Result<(), WireError> {
        self.u32(u32::try_from(data.len()).map_err(|_| WireError::ArithmeticOverflow)?)?;
        self.put(data)
    }
    pub(crate) fn finish(self) -> Result<usize, WireError> {
        if self.position == self.bytes.len() {
            Ok(self.position)
        } else {
            Err(WireError::InvalidValue)
        }
    }
}
