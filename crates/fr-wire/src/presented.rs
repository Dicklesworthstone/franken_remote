//! `PresentedState` v1: exact source identity and conservative viewer age.
//! Only a platform-qualified visible frame can yield a positive report. Other
//! decode/presentation stages are explicitly unavailable, never ready-by-default.
use crate::{
    HEADER_BYTES, Kind, Record, SourceObservation, WireError,
    decoder::{self, Binding},
    input::{InputDelivery, InputDirection},
    record::Writer,
};
use fr_core::limits::ProtocolLimits;
pub const CAPABILITY: &str = "presented-state";
pub const VERSION: u16 = 1;
pub const BYTES: usize = HEADER_BYTES + decoder::BINDING_BYTES + 43;
pub const MAX_SOURCE_AGE_US: u64 = 250_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub frame: u64,
    pub captured_us: u64,
    pub observed_us: u64,
    pub source: SourceObservation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub stamp: Stamp,
    /// Includes the full measured clock uncertainty, not half the RTT.
    pub age_upper_us: u64,
}
impl Sample {
    pub fn validate(self) -> Result<(), WireError> {
        if self.stamp.observed_us < self.stamp.captured_us
            || self.stamp.source == SourceObservation::Unknown
            || (self.stamp.source == SourceObservation::Captured
                && self.stamp.observed_us != self.stamp.captured_us)
            || self.age_upper_us >= MAX_SOURCE_AGE_US
        {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub sequence: u64,
    /// None explicitly suspends readiness. Decode/submission is not visibility.
    pub visible: Option<Sample>,
}
fn role(direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if direction != InputDirection::ViewerToHost {
        return Err(WireError::WrongRole);
    }
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    Ok(())
}
pub fn encode(
    report: Report,
    binding: Binding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    binding.validate()?;
    role(direction, delivery)?;
    if report.sequence == 0 {
        return Err(WireError::InvalidValue);
    }
    if let Some(s) = report.visible {
        s.validate()?;
    }
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        Kind::PresentedState,
        BYTES - HEADER_BYTES,
    )?;
    decoder::write_binding(&mut w, binding)?;
    w.u8(1)?;
    w.u64(report.sequence)?;
    w.u8(u8::from(report.visible.is_some()))?;
    let sample = report.visible.unwrap_or(Sample {
        stamp: Stamp {
            frame: 0,
            captured_us: 0,
            observed_us: 0,
            source: SourceObservation::Unknown,
        },
        age_upper_us: 0,
    });
    w.u64(sample.stamp.frame)?;
    w.u64(sample.stamp.captured_us)?;
    w.u64(sample.stamp.observed_us)?;
    w.u8(sample.stamp.source as u8)?;
    w.u64(sample.age_upper_us)?;
    w.finish()
}
pub fn decode(
    bytes: &[u8],
    binding: Binding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Report, WireError> {
    binding.validate()?;
    role(direction, delivery)?;
    if bytes.len() != BYTES {
        return Err(if bytes.len() < BYTES {
            WireError::Truncated
        } else {
            WireError::TrailingBytes
        });
    }
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        None,
    )?;
    let mut r = record.reader(Kind::PresentedState)?;
    decoder::check_binding(&mut r, binding)?;
    if r.u8()? != 1 {
        return Err(WireError::UnsupportedVersion);
    }
    let sequence = r.u64()?;
    if sequence == 0 {
        return Err(WireError::InvalidValue);
    }
    let stage = r.u8()?;
    let frame = r.u64()?;
    let captured_us = r.u64()?;
    let observed_us = r.u64()?;
    let source = match r.u8()? {
        0 => SourceObservation::Unknown,
        1 => SourceObservation::Captured,
        2 => SourceObservation::QualifiedUnchanged,
        _ => return Err(WireError::InvalidValue),
    };
    let age_upper_us = r.u64()?;
    let sample = Sample {
        stamp: Stamp {
            frame,
            captured_us,
            observed_us,
            source,
        },
        age_upper_us,
    };
    let visible = match stage {
        0 if frame == 0
            && captured_us == 0
            && observed_us == 0
            && source == SourceObservation::Unknown
            && age_upper_us == 0 =>
        {
            None
        }
        1 => {
            sample.validate()?;
            Some(sample)
        }
        _ => return Err(WireError::InvalidValue),
    };
    r.finish()?;
    Ok(Report { sequence, visible })
}
