//! Bounded media-configuration records on an already attached reliable channel.
//! Parsing checks declarations, NOT HEVC syntax, native resources or authority.
//! The media owner validates exact hvcC before a decoder is constructed.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration,
        ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};

pub const CAPABILITY: &str = "hevc-decoder-startup";
pub const VERSION: u16 = 1;
pub const MAX_HVCC_BYTES: usize = 23 + 3 * (5 + 4096);
pub const MAX_CODEC_BYTES: usize = 64;
pub const BINDING_BYTES: usize = 96;
pub const CONFIGURATION_OVERHEAD: usize = HEADER_BYTES + BINDING_BYTES + 31;
pub const ACK_BYTES: usize = HEADER_BYTES + BINDING_BYTES;
pub const FIRST_DECODED_BYTES: usize = ACK_BYTES + 16;

/// The installed immutable media-config tuple. `parent.id` is THIS channel's
/// compact binding, not the initial control binding. Display is an opaque local
/// display handle scoped to the host/OS-session identities, never a display name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub parent: ControlBinding,
    pub display: u128,
    pub geometry: DisplayGeometryGeneration,
    pub configuration: CodecConfigurationGeneration,
    pub recovery: RecoveryGeneration,
    pub viewport: ViewportMappingGeneration,
}
impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DecoderBinding([redacted])")
    }
}
impl Binding {
    pub fn validate(self) -> Result<(), WireError> {
        if self.parent.id == 0
            || self.parent.host_boot.as_raw() == 0
            || self.parent.os_session.as_raw() == 0
            || self.parent.remote_session.as_raw() == 0
            || self.display == 0
        {
            return Err(WireError::InvalidBinding);
        }
        Ok(())
    }
}
/// Zero-offset visible crop, independently signalled CICP color components.
/// Native baseline samples use `hev1`, with repeated parameter sets in IDRs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Configuration<'a> {
    pub coded_width: u32,
    pub coded_height: u32,
    pub crop_width: u32,
    pub crop_height: u32,
    pub fps: u16,
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
    pub decoded_pictures: u8,
    pub codec: &'a str,
    pub hvcc: &'a [u8],
}
impl fmt::Debug for Configuration<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecoderConfiguration")
            .field("hvcc_bytes", &self.hvcc.len())
            .finish_non_exhaustive()
    }
}
impl Configuration<'_> {
    pub fn validate(self, limits: &ProtocolLimits) -> Result<(), WireError> {
        limits
            .validate_coded_dimensions(self.coded_width, self.coded_height)
            .map_err(|_| WireError::ResourceLimit)?;
        if self.crop_width == 0
            || self.crop_height == 0
            || self.crop_width > self.coded_width
            || self.crop_height > self.coded_height
            || !(1..=240).contains(&self.fps)
            || !(2..=12).contains(&self.decoded_pictures)
            || !matches!(self.primaries, 1 | 9)
            || !matches!(self.transfer, 1 | 13)
            || !matches!(self.matrix, 1 | 9)
            || !self.codec.starts_with("hev1.")
            || self.codec.len() > MAX_CODEC_BYTES
            || self.codec.len() <= 5
            || !self
                .codec
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.')
        {
            return Err(WireError::InvalidValue);
        }
        if !(23..=MAX_HVCC_BYTES).contains(&self.hvcc.len()) {
            return Err(WireError::ResourceLimit);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    Configuration(Configuration<'a>),
    Configured,
    /// Decoder-local monotonic clock. This is not visibility or a host timestamp.
    FirstDecoded {
        frame: u64,
        decoder_micros: u64,
    },
}
impl Message<'_> {
    pub const fn kind(self) -> Kind {
        match self {
            Self::Configuration(_) => Kind::DecoderConfiguration,
            Self::Configured => Kind::DecoderConfigured,
            Self::FirstDecoded { .. } => Kind::FirstFrameDecoded,
        }
    }
}
fn role(kind: Kind, direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    let expected = match kind {
        Kind::DecoderConfiguration => InputDirection::HostToViewer,
        Kind::DecoderConfigured | Kind::FirstFrameDecoded => InputDirection::ViewerToHost,
        _ => return Err(WireError::UnsupportedKind),
    };
    if direction != expected {
        return Err(WireError::WrongRole);
    }
    Ok(())
}
pub(crate) fn write_binding(w: &mut Writer<'_>, b: Binding) -> Result<(), WireError> {
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
    Ok(())
}
pub(crate) fn check_binding(r: &mut Reader<'_>, b: Binding) -> Result<(), WireError> {
    for n in [
        b.parent.host_boot.as_raw(),
        b.parent.os_session.as_raw(),
        b.parent.remote_session.as_raw(),
        b.display,
    ] {
        if r.take(16)? != n.to_be_bytes() {
            return Err(WireError::InvalidBinding);
        }
    }
    for n in [
        b.geometry.as_raw(),
        b.configuration.as_raw(),
        b.recovery.as_raw(),
        b.viewport.as_raw(),
    ] {
        if r.u64()? != n {
            return Err(WireError::InvalidBinding);
        }
    }
    Ok(())
}
pub fn encode(
    message: Message<'_>,
    binding: Binding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    binding.validate()?;
    role(message.kind(), direction, delivery)?;
    let total = match message {
        Message::Configuration(c) => {
            c.validate(limits)?;
            CONFIGURATION_OVERHEAD
                .checked_add(c.codec.len())
                .and_then(|n| n.checked_add(c.hvcc.len()))
                .ok_or(WireError::ArithmeticOverflow)?
        }
        Message::Configured => ACK_BYTES,
        Message::FirstDecoded { .. } => FIRST_DECODED_BYTES,
    };
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        message.kind(),
        total - HEADER_BYTES,
    )?;
    write_binding(&mut w, binding)?;
    match message {
        Message::Configuration(c) => {
            for n in [c.coded_width, c.coded_height, c.crop_width, c.crop_height] {
                w.u32(n)?;
            }
            w.u16(c.fps)?;
            for n in [
                c.primaries,
                c.transfer,
                c.matrix,
                u8::from(c.full_range),
                c.decoded_pictures,
            ] {
                w.u8(n)?;
            }
            w.data(c.codec.as_bytes())?;
            w.data(c.hvcc)?;
        }
        Message::Configured => {}
        Message::FirstDecoded {
            frame,
            decoder_micros,
        } => {
            w.u64(frame)?;
            w.u64(decoder_micros)?;
        }
    }
    w.finish()
}
pub fn decode<'a>(
    bytes: &'a [u8],
    binding: Binding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Message<'a>, WireError> {
    binding.validate()?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        Some(crate::Channel::MediaConfig),
    )?;
    role(record.kind(), direction, delivery)?;
    let mut r = record.reader(record.kind())?;
    check_binding(&mut r, binding)?;
    let message = match record.kind() {
        Kind::DecoderConfiguration => {
            let coded_width = r.u32()?;
            let coded_height = r.u32()?;
            let crop_width = r.u32()?;
            let crop_height = r.u32()?;
            let fps = r.u16()?;
            let primaries = r.u8()?;
            let transfer = r.u8()?;
            let matrix = r.u8()?;
            let full_range = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(WireError::InvalidValue),
            };
            let decoded_pictures = r.u8()?;
            let codec = core::str::from_utf8(r.data()?).map_err(|_| WireError::InvalidValue)?;
            let c = Configuration {
                coded_width,
                coded_height,
                crop_width,
                crop_height,
                fps,
                primaries,
                transfer,
                matrix,
                full_range,
                decoded_pictures,
                codec,
                hvcc: r.data()?,
            };
            c.validate(limits)?;
            Message::Configuration(c)
        }
        Kind::DecoderConfigured => Message::Configured,
        Kind::FirstFrameDecoded => Message::FirstDecoded {
            frame: r.u64()?,
            decoder_micros: r.u64()?,
        },
        _ => return Err(WireError::UnsupportedKind),
    };
    r.finish()?;
    Ok(message)
}
