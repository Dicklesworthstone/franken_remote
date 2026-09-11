//! Private, bounded media-worker IPC. These records are NOT a network protocol.
//!
//! Parent-created pipes are the capability. A worker has one immutable role and
//! epoch, no listener, no certificate, no input lease and no approval endpoint.
//! Every request has one terminal response. Losing a response poisons the pipe;
//! do not retry work or scan for another magic value after a partial operation.
use crate::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy},
};
use core::fmt;
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::{LimitOverrides, ProtocolLimits},
};
use std::io::{self, Read, Write};

pub const HEADER_BYTES: usize = 36;
pub const UNIT_PREFIX_BYTES: usize = 40;
const CONFIG_BYTES: usize = 28;
pub mod capture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    Configure = 1,
    Capture = 2,
    Poll = 3,
    Present = 4,
    Stop = 5,
    Decode = 6,
    CaptureIfChanged = 7,
    ConfigureDecoder = 8,
    DiscoverCapture = 9,
    ConfigureCapture = 10,
    DiscoverMonitors = 11,
    ConfigureMonitor = 12,
    CheckMonitor = 13,
    Ready = 257,
    Unit = 258,
    NeedInput = 259,
    NeedDrain = 260,
    Presented = 261,
    Stopped = 262,
    Refused = 263,
    Decoded = 264,
    Unchanged = 265,
    DecoderReady = 266,
    CaptureScreens = 267,
    CaptureReady = 268,
    CaptureMonitors = 269,
    MonitorReady = 270,
    MonitorValid = 271,
}
impl Kind {
    fn parse(n: u16) -> Result<Self, Error> {
        Ok(match n {
            1 => Self::Configure,
            2 => Self::Capture,
            3 => Self::Poll,
            4 => Self::Present,
            5 => Self::Stop,
            6 => Self::Decode,
            7 => Self::CaptureIfChanged,
            8 => Self::ConfigureDecoder,
            9 => Self::DiscoverCapture,
            10 => Self::ConfigureCapture,
            11 => Self::DiscoverMonitors,
            12 => Self::ConfigureMonitor,
            13 => Self::CheckMonitor,
            257 => Self::Ready,
            258 => Self::Unit,
            259 => Self::NeedInput,
            260 => Self::NeedDrain,
            261 => Self::Presented,
            262 => Self::Stopped,
            263 => Self::Refused,
            264 => Self::Decoded,
            265 => Self::Unchanged,
            266 => Self::DecoderReady,
            267 => Self::CaptureScreens,
            268 => Self::CaptureReady,
            269 => Self::CaptureMonitors,
            270 => Self::MonitorReady,
            271 => Self::MonitorValid,
            _ => return Err(Error::Malformed),
        })
    }
    pub const fn is_request(self) -> bool {
        (self as u16) < 256
    }
    fn accepts_length(self, length: usize, limits: &ProtocolLimits) -> bool {
        match self {
            Self::Configure | Self::Ready => length == CONFIG_BYTES,
            Self::ConfigureCapture | Self::CaptureReady => length == capture::CONFIGURE_BYTES,
            Self::CaptureScreens => {
                (1 + capture::SCREEN_BYTES..=capture::MAX_CATALOG_BYTES).contains(&length)
                    && (length - 1).is_multiple_of(capture::SCREEN_BYTES)
            }
            Self::DiscoverCapture
            | Self::DiscoverMonitors
            | Self::CheckMonitor
            | Self::MonitorValid => length == 0,
            Self::CaptureMonitors => {
                (9..=capture::monitors::CATALOG_MAX_BYTES).contains(&length)
                    && length <= limits.max_control_message_bytes() as usize
            }
            Self::ConfigureMonitor | Self::MonitorReady => {
                length == capture::monitors::SELECTED_CONFIG_BYTES
                    && length <= limits.max_control_message_bytes() as usize
            }
            Self::ConfigureDecoder | Self::DecoderReady => {
                (CONFIG_BYTES + 23..=CONFIG_BYTES + crate::hevc::MAX_DECODER_RECORD_BYTES)
                    .contains(&length)
                    && length <= limits.max_control_message_bytes() as usize
            }
            Self::Capture | Self::CaptureIfChanged => length == 17,
            Self::Unchanged => length == 24,
            Self::Poll | Self::Stop | Self::NeedInput | Self::NeedDrain | Self::Stopped => {
                length == 0
            }
            Self::Presented | Self::Decoded => length == 8,
            Self::Refused => length == 2,
            Self::Unit | Self::Present | Self::Decode => {
                length > UNIT_PREFIX_BYTES
                    && length - UNIT_PREFIX_BYTES <= limits.max_encoded_access_unit_bytes() as usize
            }
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Error {
    Malformed = 1,
    ResourceLimit = 2,
    WrongEpoch = 3,
    WrongSequence = 4,
    WrongRole = 5,
    WrongState = 6,
    Unsupported = 7,
    NativeFailure = 8,
    GeometryChanged = 9,
    Io = 10,
    Allocation = 11,
}
impl Error {
    pub fn from_code(code: u16) -> Result<Self, Self> {
        Ok(match code {
            1 => Self::Malformed,
            2 => Self::ResourceLimit,
            3 => Self::WrongEpoch,
            4 => Self::WrongSequence,
            5 => Self::WrongRole,
            6 => Self::WrongState,
            7 => Self::Unsupported,
            8 => Self::NativeFailure,
            9 => Self::GeometryChanged,
            10 => Self::Io,
            11 => Self::Allocation,
            _ => return Err(Self::Malformed),
        })
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub epoch: u128,
    pub sequence: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub identity: Identity,
    pub length: usize,
}
impl Header {
    pub fn encode(self, limits: &ProtocolLimits) -> Result<[u8; HEADER_BYTES], Error> {
        self.validate(limits)?;
        let mut b = [0; HEADER_BYTES];
        b[..4].copy_from_slice(b"FRW0");
        b[6..8].copy_from_slice(&(self.kind as u16).to_be_bytes());
        b[8..24].copy_from_slice(&self.identity.epoch.to_be_bytes());
        b[24..32].copy_from_slice(&self.identity.sequence.to_be_bytes());
        b[32..36].copy_from_slice(
            &u32::try_from(self.length)
                .map_err(|_| Error::ResourceLimit)?
                .to_be_bytes(),
        );
        Ok(b)
    }
    pub fn decode(b: &[u8; HEADER_BYTES], limits: &ProtocolLimits) -> Result<Self, Error> {
        if &b[..4] != b"FRW0" || b[4..6] != [0, 0] {
            return Err(Error::Malformed);
        }
        let h = Self {
            kind: Kind::parse(u16::from_be_bytes([b[6], b[7]]))?,
            identity: Identity {
                epoch: u128::from_be_bytes(b[8..24].try_into().map_err(|_| Error::Malformed)?),
                sequence: u64::from_be_bytes(b[24..32].try_into().map_err(|_| Error::Malformed)?),
            },
            length: u32::from_be_bytes(b[32..36].try_into().map_err(|_| Error::Malformed)?)
                as usize,
        };
        h.validate(limits)?;
        Ok(h)
    }
    fn validate(self, limits: &ProtocolLimits) -> Result<(), Error> {
        if self.identity.epoch == 0 {
            return Err(Error::WrongEpoch);
        }
        if !self.kind.accepts_length(self.length, limits) {
            return Err(Error::ResourceLimit);
        }
        Ok(())
    }
}
/// No `Debug` over body bytes: replies may contain compressed desktop content.
pub struct Record {
    pub header: Header,
    body: Vec<u8>,
}
impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerRecord")
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}
impl Record {
    pub fn new(
        kind: Kind,
        identity: Identity,
        body: Vec<u8>,
        limits: &ProtocolLimits,
    ) -> Result<Self, Error> {
        let header = Header {
            kind,
            identity,
            length: body.len(),
        };
        header.validate(limits)?;
        Ok(Self { header, body })
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub fn into_body(self) -> Vec<u8> {
        self.body
    }
    /// Blocking worker-side pipe read. Parent runtime adapters use the same
    /// header validator before allocating and do NOT run this on a reactor.
    pub fn read(reader: &mut impl Read, limits: &ProtocolLimits) -> Result<Option<Self>, Error> {
        let mut bytes = [0; HEADER_BYTES];
        loop {
            match reader.read(&mut bytes[..1]) {
                Ok(0) => return Ok(None),
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Err(Error::Io),
            }
        }
        reader.read_exact(&mut bytes[1..])?;
        let header = Header::decode(&bytes, limits)?;
        let mut body = Vec::new();
        body.try_reserve_exact(header.length)
            .map_err(|_| Error::Allocation)?;
        body.resize(header.length, 0);
        reader.read_exact(&mut body)?;
        Ok(Some(Self { header, body }))
    }
    pub fn write(&self, writer: &mut impl Write, limits: &ProtocolLimits) -> Result<(), Error> {
        if self.header.length != self.body.len() {
            return Err(Error::Malformed);
        }
        writer.write_all(&self.header.encode(limits)?)?;
        writer.write_all(&self.body)?;
        writer.flush()?;
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Backend {
    Nvenc = 0,
    Vaapi = 1,
    SoftwareExplicit = 2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Capture,
    Present,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub backend: Backend,
    pub bitrate: u32,
    pub max_access_unit_bytes: u32,
    pub generation: CodecConfigurationGeneration,
}
impl Configuration {
    /// Private decoder startup binds exact hvcC to the admitted configuration.
    /// The worker independently revalidates the body before any decoder FFI.
    pub fn encode_decoder(self, record: &crate::hevc::DecoderRecord) -> Result<Vec<u8>, Error> {
        if record.generation() != self.generation {
            return Err(Error::WrongEpoch);
        }
        let mut body = self.encode()?;
        crate::hevc::HevcGuard::from_decoder_record(
            self.codec()?,
            self.limits()?,
            4,
            record.bytes(),
        )
        .map_err(|_| Error::Unsupported)?;
        body.try_reserve_exact(record.bytes().len())
            .map_err(|_| Error::Allocation)?;
        body.extend_from_slice(record.bytes());
        Ok(body)
    }
    pub fn decode_decoder(body: &[u8]) -> Result<(Self, &[u8]), Error> {
        if !Kind::ConfigureDecoder.accepts_length(body.len(), &ProtocolLimits::ABSOLUTE) {
            return Err(Error::ResourceLimit);
        }
        Ok((Self::decode(&body[..CONFIG_BYTES])?, &body[CONFIG_BYTES..]))
    }
    pub fn limits(self) -> Result<ProtocolLimits, Error> {
        ProtocolLimits::with_overrides(LimitOverrides {
            max_encoded_access_unit_bytes: Some(self.max_access_unit_bytes),
            ..LimitOverrides::default()
        })
        .map_err(|_| Error::ResourceLimit)
    }
    pub fn codec(self) -> Result<CodecConfiguration, Error> {
        if self.width < 16
            || self.height < 16
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || !(1..=240).contains(&self.fps)
            || !(10_000..=200_000_000).contains(&self.bitrate)
        {
            return Err(Error::Unsupported);
        }
        CodecConfiguration::new_baseline(
            self.generation,
            // The initial native subset uses a 16-pixel minimum coding block.
            // Actual encoder parameter sets must agree; this is not a hardware
            // capability claim. The IPC width/height remain the visible surface.
            CodedGeometry::from_visible(&self.limits()?, self.width, self.height, 16)
                .map_err(|_| Error::ResourceLimit)?,
            ColorInfo::sdr_bt709(),
            GopPolicy::baseline_for_frame_rate(u32::from(self.fps))
                .map_err(|_| Error::Unsupported)?,
        )
        .map_err(|_| Error::Unsupported)
    }
    pub fn encode(self) -> Result<Vec<u8>, Error> {
        self.codec()?;
        let mut b = Vec::with_capacity(CONFIG_BYTES);
        b.extend_from_slice(&self.width.to_be_bytes());
        b.extend_from_slice(&self.height.to_be_bytes());
        b.extend_from_slice(&self.fps.to_be_bytes());
        b.push(self.backend as u8);
        b.push(0);
        b.extend_from_slice(&self.bitrate.to_be_bytes());
        b.extend_from_slice(&self.max_access_unit_bytes.to_be_bytes());
        b.extend_from_slice(&self.generation.as_raw().to_be_bytes());
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        if b.len() != CONFIG_BYTES || b[11] != 0 {
            return Err(Error::Malformed);
        }
        let n = |start| {
            u32::from_be_bytes(b[start..start + 4].try_into().expect("fixed configuration"))
        };
        let c = Self {
            width: n(0),
            height: n(4),
            fps: u16::from_be_bytes([b[8], b[9]]),
            backend: match b[10] {
                0 => Backend::Nvenc,
                1 => Backend::Vaapi,
                2 => Backend::SoftwareExplicit,
                _ => return Err(Error::Unsupported),
            },
            bitrate: n(12),
            max_access_unit_bytes: n(16),
            generation: CodecConfigurationGeneration::from_raw(u64::from_be_bytes(
                b[20..28].try_into().map_err(|_| Error::Malformed)?,
            )),
        };
        c.codec()?;
        Ok(c)
    }
}
/// Sequencing is independent of frame IDs: a codec can need drain/input before
/// producing a frame. No operation is retried after losing its pipe response.
#[derive(Debug)]
pub struct Sequence {
    epoch: u128,
    next: Option<u64>,
}
impl Sequence {
    pub fn new(epoch: u128) -> Result<Self, Error> {
        if epoch == 0 {
            return Err(Error::WrongEpoch);
        }
        Ok(Self {
            epoch,
            next: Some(0),
        })
    }
    pub fn accept(&mut self, header: Header) -> Result<(), Error> {
        if header.identity.epoch != self.epoch {
            return Err(Error::WrongEpoch);
        }
        if Some(header.identity.sequence) != self.next {
            return Err(Error::WrongSequence);
        }
        self.next = header.identity.sequence.checked_add(1);
        Ok(())
    }
}
/// A completed full-source comparison, not a heartbeat. The candidate identity
/// belongs to the request; `reference` is the last picture actually encoded.
/// Only the parent bound to this worker may turn it into network progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnchangedCapture {
    pub candidate: FrameId,
    pub reference: FrameId,
    pub observed_micros: u64,
}
impl UnchangedCapture {
    pub fn encode(self) -> Result<Vec<u8>, Error> {
        if self.reference >= self.candidate {
            return Err(Error::Malformed);
        }
        let mut bytes = Vec::with_capacity(24);
        bytes.extend_from_slice(&self.candidate.as_raw().to_be_bytes());
        bytes.extend_from_slice(&self.reference.as_raw().to_be_bytes());
        bytes.extend_from_slice(&self.observed_micros.to_be_bytes());
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let b: &[u8; 24] = bytes.try_into().map_err(|_| Error::Malformed)?;
        let value = Self {
            candidate: FrameId::from_raw(u64::from_be_bytes(b[..8].try_into().unwrap())),
            reference: FrameId::from_raw(u64::from_be_bytes(b[8..16].try_into().unwrap())),
            observed_micros: u64::from_be_bytes(b[16..].try_into().unwrap()),
        };
        if value.reference >= value.candidate {
            return Err(Error::Malformed);
        }
        Ok(value)
    }
}

pub fn capture_payload(frame: FrameId, capture_lower_bound: u64, force_idr: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(17);
    b.extend_from_slice(&frame.as_raw().to_be_bytes());
    b.extend_from_slice(&capture_lower_bound.to_be_bytes());
    b.push(u8::from(force_idr));
    b
}
pub fn parse_capture(b: &[u8]) -> Result<(FrameId, u64, bool), Error> {
    if b.len() != 17 || b[16] > 1 {
        return Err(Error::Malformed);
    }
    Ok((FrameId::from_raw(u64_at(b, 0)?), u64_at(b, 8)?, b[16] == 1))
}
pub fn unit_payload(unit: &EncodedAccessUnit) -> Result<Vec<u8>, Error> {
    unit_parts(
        unit.frame(),
        unit.capture_micros(),
        unit.config_generation(),
        unit.kind(),
        unit.bytes(),
    )
}
/// Serialize directly from a bounded received picture without cloning its AU.
pub fn unit_parts(
    frame: FrameId,
    capture: u64,
    config: CodecConfigurationGeneration,
    kind: FrameKind,
    bytes: &[u8],
) -> Result<Vec<u8>, Error> {
    ProtocolLimits::ABSOLUTE
        .validate_access_unit_len(bytes.len())
        .map_err(|_| Error::ResourceLimit)?;
    let mut b = Vec::new();
    let length = UNIT_PREFIX_BYTES
        .checked_add(bytes.len())
        .ok_or(Error::ResourceLimit)?;
    b.try_reserve_exact(length).map_err(|_| Error::Allocation)?;
    b.extend_from_slice(&frame.as_raw().to_be_bytes());
    b.extend_from_slice(&capture.to_be_bytes());
    b.extend_from_slice(&config.as_raw().to_be_bytes());
    let (kind, reference) = match kind {
        FrameKind::Idr { recovery } => (0, recovery.as_raw()),
        FrameKind::Predicted { references } => (1, references.as_raw()),
    };
    b.extend_from_slice(&reference.to_be_bytes());
    b.push(kind);
    b.extend_from_slice(&[0; 7]);
    b.extend_from_slice(bytes);
    Ok(b)
}
pub fn parse_unit(b: Vec<u8>, limits: &ProtocolLimits) -> Result<EncodedAccessUnit, Error> {
    if b.len() <= UNIT_PREFIX_BYTES || b[33..40] != [0; 7] {
        return Err(Error::Malformed);
    }
    let frame = FrameId::from_raw(u64_at(&b, 0)?);
    let reference = u64_at(&b, 24)?;
    let kind = match b[32] {
        0 => FrameKind::Idr {
            recovery: RecoveryGeneration::from_raw(reference),
        },
        1 if reference < frame.as_raw() => FrameKind::Predicted {
            references: FrameId::from_raw(reference),
        },
        _ => return Err(Error::Malformed),
    };
    let capture = u64_at(&b, 8)?;
    let configuration = CodecConfigurationGeneration::from_raw(u64_at(&b, 16)?);
    limits
        .validate_access_unit_len(b.len() - UNIT_PREFIX_BYTES)
        .map_err(|_| Error::ResourceLimit)?;
    // Shift in-place: avoid retaining a second AU-sized allocation at this IPC boundary.
    let mut bytes = b;
    bytes.copy_within(UNIT_PREFIX_BYTES.., 0);
    bytes.truncate(bytes.len() - UNIT_PREFIX_BYTES);
    EncodedAccessUnit::new(limits, frame, kind, configuration, capture, bytes)
        .map_err(|_| Error::Malformed)
}
fn u64_at(b: &[u8], offset: usize) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        b.get(offset..offset + 8)
            .ok_or(Error::Malformed)?
            .try_into()
            .map_err(|_| Error::Malformed)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_sequence_never_wraps() {
        let mut s = Sequence {
            epoch: 1,
            next: Some(u64::MAX),
        };
        let h = Header {
            kind: Kind::Poll,
            identity: Identity {
                epoch: 1,
                sequence: u64::MAX,
            },
            length: 0,
        };
        s.accept(h).unwrap();
        assert_eq!(s.accept(h), Err(Error::WrongSequence));
        assert_eq!(
            s.accept(Header {
                identity: Identity {
                    epoch: 1,
                    sequence: 0
                },
                ..h
            }),
            Err(Error::WrongSequence)
        );
    }
}
