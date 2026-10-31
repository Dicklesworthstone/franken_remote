//! Point-in-time metadata from one live local viewer, never input authority.
//!
//! A receipt's local age is NOT source freshness. No pixels, host wall-clock
//! conversion, codec generation, lease or semantic application result is inferred.
use fr_core::limits::ProtocolLimits;
use fr_wire::display::Display;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Connecting,
    Authenticated,
    AwaitingApproval,
    Streaming,
    Cleaning,
    Reconnecting,
    Stopped,
}

/// These are the only receipts the native presentation API supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Decoded,
    SubmittedToCompositor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    /// Canonical 32-digit hexadecimal handle, not an index or an OS identifier.
    pub display_handle: String,
    pub geometry_generation: u64,
    pub origin_x: i32,
    pub origin_y: i32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub scale_numerator: u32,
    pub scale_denominator: u32,
    pub rotation_quarter_turns: u8,
    pub frame_serial: u64,
    pub evidence_level: Evidence,
    /// Monotonic age at the local owner's snapshot, NOT host capture age.
    pub receipt_age_ms: u64,
}
impl Frame {
    pub fn validate(&self) -> Result<(), Error> {
        if self.display_handle.len() != 32
            || !self
                .display_handle
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || u128::from_str_radix(&self.display_handle, 16)
                .ok()
                .is_none_or(|id| id == 0)
        {
            return Err(Error::InvalidMetadata);
        }
        for (width, height) in [
            (self.pixel_width, self.pixel_height),
            (self.logical_width, self.logical_height),
        ] {
            ProtocolLimits::ABSOLUTE
                .validate_coded_dimensions(width, height)
                .map_err(|_| Error::InvalidMetadata)?;
        }
        for (origin, size) in [
            (self.origin_x, self.pixel_width),
            (self.origin_y, self.pixel_height),
        ] {
            i32::try_from(i64::from(origin) + i64::from(size) - 1)
                .map_err(|_| Error::InvalidMetadata)?;
        }
        let (n, d) = (self.scale_numerator, self.scale_denominator);
        if !(1..=65_536).contains(&n)
            || !(1..=65_536).contains(&d)
            || u64::from(n) > 16 * u64::from(d)
            || u64::from(d) > 16 * u64::from(n)
            || self.rotation_quarter_turns > 3
        {
            return Err(Error::InvalidMetadata);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema_version: u8,
    /// Changes on reconnect even when the local connection ID stays unchanged.
    pub attempt: u8,
    pub phase: Phase,
    pub frame: Option<Frame>,
}
impl Snapshot {
    /// Validate a bounded IPC payload after decoding, before exposing any data.
    pub fn validate(&self) -> Result<(), Error> {
        if self.schema_version != 1
            || self.attempt > 32
            || (self.attempt == 0 && !matches!(self.phase, Phase::Starting | Phase::Stopped))
            || (self.phase == Phase::Starting && self.attempt != 0)
            || (self.phase == Phase::Streaming) != self.frame.is_some()
        {
            return Err(Error::InvalidMetadata);
        }
        if let Some(frame) = &self.frame {
            frame.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Order,
    Clock,
    InvalidMetadata,
}

/// One bounded slot. Native callbacks update Copy metadata without allocating;
/// serialization and display-handle formatting happen only for an explicit read.
pub struct Observation {
    attempt: u8,
    phase: Phase,
    frame: Option<(Display, u64, Evidence, Instant)>,
}
impl Default for Observation {
    fn default() -> Self {
        Self {
            attempt: 0,
            phase: Phase::Starting,
            frame: None,
        }
    }
}
impl Observation {
    pub fn begin(&mut self, attempt: u8) -> Result<(), Error> {
        self.frame = None;
        if !(1..=32).contains(&attempt) || attempt <= self.attempt {
            self.phase = Phase::Stopped;
            return Err(Error::Order);
        }
        self.attempt = attempt;
        self.phase = Phase::Connecting;
        Ok(())
    }
    pub fn authenticated(&mut self) -> Result<(), Error> {
        self.change(Phase::Connecting, Phase::Authenticated)
    }
    pub fn approval(&mut self) -> Result<(), Error> {
        if self.phase == Phase::AwaitingApproval {
            return Ok(());
        }
        self.change(Phase::Authenticated, Phase::AwaitingApproval)
    }
    fn change(&mut self, from: Phase, to: Phase) -> Result<(), Error> {
        if self.phase != from {
            self.retire(Phase::Stopped);
            return Err(Error::Order);
        }
        self.phase = to;
        Ok(())
    }
    /// Publish only the original selected display and its actual initial receipt.
    pub fn ready(
        &mut self,
        display: Display,
        serial: u64,
        evidence: Evidence,
        at: Instant,
    ) -> Result<(), Error> {
        if !matches!(self.phase, Phase::Authenticated | Phase::AwaitingApproval) {
            self.retire(Phase::Stopped);
            return Err(Error::Order);
        }
        if display.validate(&ProtocolLimits::ABSOLUTE).is_err() {
            self.retire(Phase::Stopped);
            return Err(Error::InvalidMetadata);
        }
        self.frame = Some((display, serial, evidence, at));
        self.phase = Phase::Streaming;
        Ok(())
    }
    pub fn presented(&mut self, serial: u64, evidence: Evidence, at: Instant) -> Result<(), Error> {
        let Some((display, previous_serial, previous_evidence, previous)) = self.frame else {
            return Err(Error::Order);
        };
        if self.phase != Phase::Streaming || at < previous {
            self.retire(Phase::Stopped);
            return Err(Error::Clock);
        }
        // Startup can deliver the retained first receipt to the service callback
        // too. Seeing it twice is not a new decode and cannot refresh its age.
        if serial != previous_serial || evidence != previous_evidence {
            self.frame = Some((display, serial, evidence, at));
        }
        Ok(())
    }
    /// Clear BEFORE teardown/backoff. No old geometry or receipt crosses attempts.
    pub fn retire(&mut self, phase: Phase) {
        self.frame = None;
        self.phase = if matches!(
            phase,
            Phase::Cleaning | Phase::Reconnecting | Phase::Stopped
        ) && (self.attempt != 0 || phase == Phase::Stopped)
        {
            phase
        } else {
            Phase::Stopped
        };
    }
    pub fn snapshot(&self, at: Instant) -> Result<Snapshot, Error> {
        let frame = self
            .frame
            .map(|(d, serial, evidence, receipt)| {
                let age = at.checked_duration_since(receipt).ok_or(Error::Clock)?;
                Ok::<Frame, Error>(Frame {
                    display_handle: format!("{:032x}", d.handle),
                    geometry_generation: d.geometry.as_raw(),
                    origin_x: d.x,
                    origin_y: d.y,
                    pixel_width: d.pixel_width,
                    pixel_height: d.pixel_height,
                    logical_width: d.logical_width,
                    logical_height: d.logical_height,
                    scale_numerator: d.scale_numerator,
                    scale_denominator: d.scale_denominator,
                    rotation_quarter_turns: d.rotation,
                    frame_serial: serial,
                    evidence_level: evidence,
                    receipt_age_ms: u64::try_from(age.as_nanos().div_ceil(1_000_000))
                        .map_err(|_| Error::Clock)?,
                })
            })
            .transpose()?;
        Ok(Snapshot {
            schema_version: 1,
            attempt: self.attempt,
            phase: self.phase,
            frame,
        })
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
