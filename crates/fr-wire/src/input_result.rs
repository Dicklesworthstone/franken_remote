//! Content-free terminal input receipts. See `PROTOCOL_INPUT.md` for the v0
//! layout. Parsing authenticates no peer and never authorizes or retries input.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    authority::AuthorityError,
    ids::{InputLeaseId, RemoteSessionId},
    input::MAX_COMMITTED_TEXT_BYTES,
    input_sequence::{InputOutcome, InputSequenceError},
    input_submission::{PlatformError, Receipt, Refusal},
    limits::ProtocolLimits,
};

pub const INPUT_RESULT_BYTES: usize = HEADER_BYTES + 50;

/// Installed by the caller after channel attachment. IDs alone grant nothing.
/// The session also binds host boot and OS session; reconnect cannot reuse it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ResultBinding {
    pub channel: u32,
    pub session: RemoteSessionId,
    pub lease: InputLeaseId,
}
impl fmt::Debug for ResultBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResultBinding")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SequenceSpace {
    Action = 0,
    Pointer = 1,
}

/// Acknowledgement stage, never an exactly-once or rollback claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    Admitted = 0,
    SubmittedToOs = 1,
    Observed = 2,
}

/// Stable v0 refusal categories. Internal error details never cross the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Reason {
    InvalidState = 1,
    ObservationExpired = 2,
    NoLease = 3,
    StaleLease = 4,
    LeaseExpired = 5,
    ViewUnready = 6,
    ControllerBusy = 7,
    ControllerCleanupRequired = 8,
    TicketInvalid = 9,
    TicketExpired = 10,
    ChallengeMismatch = 11,
    ChallengePending = 12,
    ChallengeExpired = 13,
    ClockRegression = 14,
    DeadlineOverflow = 15,
    InvalidReceiptCapacity = 16,
    SequenceFenced = 17,
    PreviousActionPending = 18,
    SequenceGap = 19,
    NotPending = 20,
    StaleSession = 21,
    StaleView = 22,
    OutOfBounds = 23,
    Unsupported = 24,
    InvalidTransition = 25,
    RelativeOverflow = 26,
    ModeMismatch = 27,
    Revoked = 28,
    PermissionMissing = 29,
    PlatformGeometryChanged = 30,
    PlatformUnavailable = 31,
    UnknownEffect = 32,
    AuthorityUnavailable = 33,
}
impl Reason {
    fn parse(code: u16) -> Result<Option<Self>, WireError> {
        Ok(Some(match code {
            0 => return Ok(None),
            1 => Self::InvalidState,
            2 => Self::ObservationExpired,
            3 => Self::NoLease,
            4 => Self::StaleLease,
            5 => Self::LeaseExpired,
            6 => Self::ViewUnready,
            7 => Self::ControllerBusy,
            8 => Self::ControllerCleanupRequired,
            9 => Self::TicketInvalid,
            10 => Self::TicketExpired,
            11 => Self::ChallengeMismatch,
            12 => Self::ChallengePending,
            13 => Self::ChallengeExpired,
            14 => Self::ClockRegression,
            15 => Self::DeadlineOverflow,
            16 => Self::InvalidReceiptCapacity,
            17 => Self::SequenceFenced,
            18 => Self::PreviousActionPending,
            19 => Self::SequenceGap,
            20 => Self::NotPending,
            21 => Self::StaleSession,
            22 => Self::StaleView,
            23 => Self::OutOfBounds,
            24 => Self::Unsupported,
            25 => Self::InvalidTransition,
            26 => Self::RelativeOverflow,
            27 => Self::ModeMismatch,
            28 => Self::Revoked,
            29 => Self::PermissionMissing,
            30 => Self::PlatformGeometryChanged,
            31 => Self::PlatformUnavailable,
            32 => Self::UnknownEffect,
            33 => Self::AuthorityUnavailable,
            _ => return Err(WireError::InvalidValue),
        }))
    }
    const fn is_expiry(self) -> bool {
        matches!(
            self,
            Self::ObservationExpired | Self::LeaseExpired | Self::TicketExpired
        )
    }
}
impl TryFrom<Refusal> for Reason {
    type Error = WireError;
    fn try_from(refusal: Refusal) -> Result<Self, WireError> {
        Ok(match refusal {
            Refusal::Authority(error) => match error {
                AuthorityError::InvalidState { .. } => Self::InvalidState,
                AuthorityError::ObservationExpired => Self::ObservationExpired,
                AuthorityError::NoLease => Self::NoLease,
                AuthorityError::StaleLease => Self::StaleLease,
                AuthorityError::LeaseExpired => Self::LeaseExpired,
                AuthorityError::ViewUnready => Self::ViewUnready,
                AuthorityError::ControllerBusy => Self::ControllerBusy,
                AuthorityError::ControllerCleanupRequired => Self::ControllerCleanupRequired,
                AuthorityError::TicketInvalid => Self::TicketInvalid,
                AuthorityError::TicketExpired => Self::TicketExpired,
                AuthorityError::ChallengeMismatch => Self::ChallengeMismatch,
                AuthorityError::ChallengePending => Self::ChallengePending,
                AuthorityError::ChallengeExpired => Self::ChallengeExpired,
                AuthorityError::ClockRegression => Self::ClockRegression,
                AuthorityError::DeadlineOverflow => Self::DeadlineOverflow,
                _ => return Err(WireError::UnsupportedKind),
            },
            Refusal::Sequence(error) => match error {
                InputSequenceError::InvalidCapacity { .. } => Self::InvalidReceiptCapacity,
                InputSequenceError::StaleLease => Self::StaleLease,
                InputSequenceError::Fenced => Self::SequenceFenced,
                InputSequenceError::PreviousActionPending { .. } => Self::PreviousActionPending,
                InputSequenceError::SequenceGap { .. } => Self::SequenceGap,
                InputSequenceError::NotPending => Self::NotPending,
                _ => return Err(WireError::UnsupportedKind),
            },
            Refusal::StaleSession => Self::StaleSession,
            Refusal::StaleLease => Self::StaleLease,
            Refusal::StaleView => Self::StaleView,
            Refusal::OutOfBounds => Self::OutOfBounds,
            Refusal::Unsupported | Refusal::Platform(PlatformError::Unsupported) => {
                Self::Unsupported
            }
            Refusal::InvalidTransition => Self::InvalidTransition,
            Refusal::RelativeOverflow => Self::RelativeOverflow,
            Refusal::ModeMismatch => Self::ModeMismatch,
            Refusal::Revoked => Self::Revoked,
            Refusal::Platform(PlatformError::Permission) => Self::PermissionMissing,
            Refusal::Platform(PlatformError::GeometryChanged) => Self::PlatformGeometryChanged,
            Refusal::Platform(PlatformError::Unavailable) => Self::PlatformUnavailable,
            Refusal::UnknownEffect => Self::UnknownEffect,
            Refusal::AuthorityUnavailable => Self::AuthorityUnavailable,
        })
    }
}

/// Fixed-size metadata, with no ticket, coordinates, key identity or text.
/// `unknown_next_operation` describes the operation after the confirmed prefix;
/// later operations were not attempted. Cleanup does not subtract that prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputResult {
    pub binding: ResultBinding,
    pub sequence: u64,
    pub space: SequenceSpace,
    pub stage: Stage,
    pub outcome: InputOutcome,
    pub submitted_operations: u32,
    pub unknown_next_operation: bool,
    pub reason: Option<Reason>,
}
impl InputResult {
    /// Preserve an actual terminal receipt. A missing/evicted receipt cannot
    /// enter this API as a fabricated zero-effect success or rejection.
    /// The caller supplies the original action's sequence space and binding.
    pub fn from_receipt(
        binding: ResultBinding,
        space: SequenceSpace,
        receipt: Receipt,
    ) -> Result<Self, WireError> {
        let result = Self {
            binding,
            sequence: receipt.sequence,
            space,
            stage: if receipt.submitted_operations == 0 {
                Stage::Admitted
            } else {
                Stage::SubmittedToOs
            },
            outcome: receipt.outcome,
            submitted_operations: receipt.submitted_operations,
            unknown_next_operation: receipt.outcome == InputOutcome::EffectUnknown,
            reason: receipt.refusal.map(Reason::try_from).transpose()?,
        };
        result.validate()?;
        Ok(result)
    }

    fn validate(self) -> Result<(), WireError> {
        if self.binding.channel == 0 {
            return Err(WireError::InvalidBinding);
        }
        // One scalar per byte is the largest possible text operation count.
        // All other implemented actions require at most two native operations.
        let maximum =
            u32::try_from(MAX_COMMITTED_TEXT_BYTES).map_err(|_| WireError::ArithmeticOverflow)?;
        if self.submitted_operations > maximum
            || ((self.unknown_next_operation
                || self.outcome == InputOutcome::PartiallySubmittedToOs)
                && self.submitted_operations == maximum)
        {
            return Err(WireError::ResourceLimit);
        }
        let has_prefix = self.submitted_operations != 0;
        let unknown = self.outcome == InputOutcome::EffectUnknown;
        if self.space == SequenceSpace::Pointer
            && (self.submitted_operations > 1
                || (unknown && has_prefix)
                || matches!(
                    self.outcome,
                    InputOutcome::AppliedLocally | InputOutcome::PartiallySubmittedToOs
                ))
        {
            return Err(WireError::InvalidValue);
        }
        if self.unknown_next_operation != unknown
            || (self.stage == Stage::Admitted) == has_prefix
            || (self.stage == Stage::Observed && self.outcome != InputOutcome::SubmittedToOs)
        {
            return Err(WireError::InvalidValue);
        }
        let valid = match self.outcome {
            InputOutcome::SubmittedToOs => has_prefix && self.reason.is_none(),
            InputOutcome::AppliedLocally => !has_prefix && self.reason.is_none(),
            InputOutcome::RejectedBeforeSubmission => {
                !has_prefix
                    && self.reason.is_some_and(|reason| {
                        !reason.is_expiry()
                            && !matches!(reason, Reason::Revoked | Reason::UnknownEffect)
                    })
            }
            InputOutcome::ExpiredBeforeSubmission => {
                !has_prefix && self.reason.is_some_and(Reason::is_expiry)
            }
            InputOutcome::CancelledBeforeSubmission => {
                !has_prefix && self.reason == Some(Reason::Revoked)
            }
            InputOutcome::PartiallySubmittedToOs => {
                has_prefix
                    && self
                        .reason
                        .is_some_and(|reason| reason != Reason::UnknownEffect)
            }
            InputOutcome::EffectUnknown => self.reason == Some(Reason::UnknownEffect),
        };
        if valid {
            Ok(())
        } else {
            Err(WireError::InvalidValue)
        }
    }
}

fn route(direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if direction != InputDirection::HostToViewer {
        return Err(WireError::WrongRole);
    }
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    Ok(())
}
fn opaque(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}

/// Validate one whole record without allocation. The caller still checks live
/// session/closing-drain admission and retains only a bounded receipt window.
pub fn decode_input_result(
    bytes: &[u8],
    limits: &ProtocolLimits,
    binding: ResultBinding,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<InputResult, WireError> {
    route(direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.channel,
        None,
    )?;
    let mut r = record.reader(Kind::InputResult)?;
    if RemoteSessionId::from_raw(opaque(&mut r)?) != binding.session
        || InputLeaseId::from_raw(opaque(&mut r)?) != binding.lease
    {
        return Err(WireError::InvalidBinding);
    }
    let result = InputResult {
        binding,
        sequence: r.u64()?,
        space: match r.u8()? {
            0 => SequenceSpace::Action,
            1 => SequenceSpace::Pointer,
            _ => return Err(WireError::InvalidValue),
        },
        stage: match r.u8()? {
            0 => Stage::Admitted,
            1 => Stage::SubmittedToOs,
            2 => Stage::Observed,
            _ => return Err(WireError::InvalidValue),
        },
        outcome: match r.u8()? {
            0 => InputOutcome::SubmittedToOs,
            1 => InputOutcome::AppliedLocally,
            2 => InputOutcome::RejectedBeforeSubmission,
            3 => InputOutcome::ExpiredBeforeSubmission,
            4 => InputOutcome::CancelledBeforeSubmission,
            5 => InputOutcome::PartiallySubmittedToOs,
            6 => InputOutcome::EffectUnknown,
            _ => return Err(WireError::InvalidValue),
        },
        submitted_operations: r.u32()?,
        unknown_next_operation: match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(WireError::InvalidValue),
        },
        reason: Reason::parse(r.u16()?)?,
    };
    r.finish()?;
    result.validate()?;
    Ok(result)
}

/// Encode into caller-owned bounded storage. Validation precedes every write.
pub fn encode_input_result(
    result: InputResult,
    out: &mut [u8],
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    route(direction, delivery)?;
    result.validate()?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        result.binding.channel,
        Kind::InputResult,
        INPUT_RESULT_BYTES - HEADER_BYTES,
    )?;
    w.put(&result.binding.session.as_raw().to_be_bytes())?;
    w.put(&result.binding.lease.as_raw().to_be_bytes())?;
    w.u64(result.sequence)?;
    w.u8(result.space as u8)?;
    w.u8(result.stage as u8)?;
    w.u8(match result.outcome {
        InputOutcome::SubmittedToOs => 0,
        InputOutcome::AppliedLocally => 1,
        InputOutcome::RejectedBeforeSubmission => 2,
        InputOutcome::ExpiredBeforeSubmission => 3,
        InputOutcome::CancelledBeforeSubmission => 4,
        InputOutcome::PartiallySubmittedToOs => 5,
        InputOutcome::EffectUnknown => 6,
    })?;
    w.u32(result.submitted_operations)?;
    w.u8(u8::from(result.unknown_next_operation))?;
    w.u16(result.reason.map_or(0, |reason| reason as u16))?;
    w.finish()
}
