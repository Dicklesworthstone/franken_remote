//! Negotiated, content-free reports from the original host capture owner.
//! Cadence and applied encoder bitrate describe separate controls. A pending
//! bitrate is intent, never confirmation. These records cannot reconfigure a
//! decoder, refresh a view, grant input authority or extend any deadline.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    decoder::{self, Binding},
    input::{InputDelivery, InputDirection},
    negotiation::Selection,
    record::{Reader, Writer},
};
use fr_core::limits::ProtocolLimits;

pub const CAPABILITY: &str = "quality-decisions";
pub const VERSION: u16 = 1;
pub const RECORD_BYTES: usize = HEADER_BYTES + decoder::BINDING_BYTES + 30;
pub const MIN_CAPTURE_INTERVAL_US: u32 = 1_000;
pub const MAX_CAPTURE_INTERVAL_US: u32 = 1_000_000;
pub const MIN_BITRATE_BPS: u32 = 10_000;
pub const MAX_BITRATE_BPS: u32 = 200_000_000;

/// Positive agreement about reporting only, in either observation/control role.
/// Callers still check the original session, channel and observation authority.
pub fn selected(selection: &Selection) -> bool {
    selection.validate().is_ok()
        && [
            (CAPABILITY, VERSION),
            (decoder::CAPABILITY, decoder::VERSION),
        ]
        .iter()
        .all(|&(name, version)| {
            selection
                .capabilities
                .iter()
                .any(|cap| cap.name == name && cap.version == version)
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    Active = 0,
    Idle = 1,
}
impl Mode {
    fn parse(value: u8) -> Result<Self, WireError> {
        match value {
            0 => Ok(Self::Active),
            1 => Ok(Self::Idle),
            _ => Err(WireError::InvalidValue),
        }
    }
}

/// The host's bounded pacing reason. Send admission is not a bandwidth
/// measurement; idle means an actual unchanged source observation was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Reason {
    Hold = 0,
    SourceWork = 1,
    SendAdmission = 2,
    CaptureCredit = 3,
    ReceiverBacklog = 4,
    DecoderWork = 5,
    HeadroomProbe = 6,
    VerifiedIdle = 7,
    ChangedAfterIdle = 8,
    EvidenceGap = 9,
}
impl Reason {
    fn parse(value: u8) -> Result<Self, WireError> {
        match value {
            0 => Ok(Self::Hold),
            1 => Ok(Self::SourceWork),
            2 => Ok(Self::SendAdmission),
            3 => Ok(Self::CaptureCredit),
            4 => Ok(Self::ReceiverBacklog),
            5 => Ok(Self::DecoderWork),
            6 => Ok(Self::HeadroomProbe),
            7 => Ok(Self::VerifiedIdle),
            8 => Ok(Self::ChangedAfterIdle),
            9 => Ok(Self::EvidenceGap),
            _ => Err(WireError::InvalidValue),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Nonzero, strictly increasing within the original reporting owner.
    /// The receiver checks ordering; decoding alone never makes this fresh.
    pub sequence: u64,
    /// Selected source admission interval, not measured frame delivery rate.
    /// The wire uses u64 microseconds; the admitted range fits this checked u32.
    pub capture_interval_us: u32,
    pub mode: Mode,
    pub reason: Reason,
    /// The original encoder's acknowledged setting, not measured output rate.
    /// None means the source owner has no applied-bitrate evidence to report.
    pub applied_bitrate_bps: Option<u32>,
    /// An unapplied target retained by that same owner. Requires applied state.
    pub pending_bitrate_bps: Option<u32>,
}
impl Decision {
    pub fn validate(self) -> Result<(), WireError> {
        if self.sequence == 0
            || !(MIN_CAPTURE_INTERVAL_US..=MAX_CAPTURE_INTERVAL_US)
                .contains(&self.capture_interval_us)
            || (self.pending_bitrate_bps.is_some() && self.applied_bitrate_bps.is_none())
            || [self.applied_bitrate_bps, self.pending_bitrate_bps]
                .into_iter()
                .flatten()
                .any(|rate| !(MIN_BITRATE_BPS..=MAX_BITRATE_BPS).contains(&rate))
        {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}

fn role(direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    if direction != InputDirection::HostToViewer {
        return Err(WireError::WrongRole);
    }
    Ok(())
}

pub fn encode(
    decision: Decision,
    binding: Binding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    binding.validate()?;
    role(direction, delivery)?;
    decision.validate()?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        Kind::QualityDecision,
        RECORD_BYTES - HEADER_BYTES,
    )?;
    decoder::write_binding(&mut w, binding)?;
    w.u16(VERSION)?;
    w.u64(decision.sequence)?;
    w.u64(u64::from(decision.capture_interval_us))?;
    w.u8(decision.mode as u8)?;
    w.u8(decision.reason as u8)?;
    for rate in [decision.applied_bitrate_bps, decision.pending_bitrate_bps] {
        w.u8(u8::from(rate.is_some()))?;
        w.u32(rate.unwrap_or(0))?;
    }
    w.finish()
}

fn bitrate(reader: &mut Reader<'_>) -> Result<Option<u32>, WireError> {
    let known = reader.u8()?;
    let rate = reader.u32()?;
    match (known, rate) {
        (0, 0) => Ok(None),
        (1, rate) if (MIN_BITRATE_BPS..=MAX_BITRATE_BPS).contains(&rate) => Ok(Some(rate)),
        _ => Err(WireError::InvalidValue),
    }
}

pub fn decode(
    bytes: &[u8],
    binding: Binding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Decision, WireError> {
    binding.validate()?;
    role(direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        None,
    )?;
    let mut r = record.reader(Kind::QualityDecision)?;
    decoder::check_binding(&mut r, binding)?;
    if r.u16()? != VERSION {
        return Err(WireError::UnsupportedVersion);
    }
    let sequence = r.u64()?;
    let interval = r.u64()?;
    if !(u64::from(MIN_CAPTURE_INTERVAL_US)..=u64::from(MAX_CAPTURE_INTERVAL_US))
        .contains(&interval)
    {
        return Err(WireError::InvalidValue);
    }
    let decision = Decision {
        sequence,
        capture_interval_us: u32::try_from(interval).map_err(|_| WireError::InvalidValue)?,
        mode: Mode::parse(r.u8()?)?,
        reason: Reason::parse(r.u8()?)?,
        applied_bitrate_bps: bitrate(&mut r)?,
        pending_bitrate_bps: bitrate(&mut r)?,
    };
    decision.validate()?;
    r.finish()?;
    Ok(decision)
}
