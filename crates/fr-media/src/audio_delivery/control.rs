//! One pre-admission audio control record, retained unchanged through backpressure.
//!
//! This is not transport delivery or authority. The owner rechecks observation
//! permission at the send boundary, consumes this offer only on actual transport
//! admission, and closes its optional audio lane if the original lifetime ends.
use super::LaneAction;
use fr_wire::audio as wire;

/// Original admission lifetime of a configuration or stop record.
pub const CONTROL_SEND_US: u64 = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    WrongAction,
    InvalidRecord,
    StaleOffer,
    ClockOverflow,
    ClockRegression,
    Expired,
}

/// At most one fixed-size control record per audio lane. Packet payloads never
/// enter this owner; their capture-anchored datagram lifetime is independent.
#[derive(Debug)]
pub struct ControlOffer {
    action: LaneAction,
    binding: u32,
    bytes: [u8; wire::AUDIO_CONFIGURATION_RECORD_BYTES],
    length: usize,
    deadline: u64,
    last_now: u64,
    failure: Option<Error>,
}

impl ControlOffer {
    pub fn prepare(action: LaneAction, binding: u32, now: u64) -> Result<Self, Error> {
        let deadline = now
            .checked_add(CONTROL_SEND_US)
            .ok_or(Error::ClockOverflow)?;
        let mut bytes = [0; wire::AUDIO_CONFIGURATION_RECORD_BYTES];
        let length = match action {
            LaneAction::Configure(configuration) => {
                wire::encode_configuration(&configuration, binding, &mut bytes)
            }
            LaneAction::Stop(stop) => wire::encode_stop(&stop, binding, &mut bytes),
            LaneAction::Nothing | LaneAction::Packet { .. } => return Err(Error::WrongAction),
        }
        .map_err(|_| Error::InvalidRecord)?;
        Ok(Self {
            action,
            binding,
            bytes,
            length,
            deadline,
            last_now: now,
            failure: None,
        })
    }

    /// A source or viewer state change may supersede an UNADMITTED offer only.
    /// Matching the complete action prevents a different epoch, shape or stop
    /// reason from being sent under an earlier operation's bytes.
    pub fn matches(&self, action: LaneAction, binding: u32) -> bool {
        self.action == action && self.binding == binding
    }

    /// Retry permission never renews the deadline. Expiry and clock failure are
    /// terminal for this offer, even if a caller later supplies an older clock.
    pub fn authorize(&mut self, action: LaneAction, binding: u32, now: u64) -> Result<(), Error> {
        if !self.matches(action, binding) {
            return Err(Error::StaleOffer);
        }
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        let failure = if now < self.last_now {
            Some(Error::ClockRegression)
        } else if now >= self.deadline {
            Some(Error::Expired)
        } else {
            None
        };
        self.last_now = now;
        self.failure = failure;
        failure.map_or(Ok(()), Err)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }

    pub const fn deadline_micros(&self) -> u64 {
        self.deadline
    }
}

#[cfg(test)]
mod tests;
