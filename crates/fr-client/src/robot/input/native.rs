//! A single original native owner's bounded, content-free request ledger.
//!
//! Authentication, observation preconditions, action encoding and transport stay
//! with that owner. This ledger cannot grant input, renew a ticket or retry work.
//! Encoding is already a possible send: a missing receipt is uncertainty, never
//! a zero-effect refusal. The consumed request floor outlives receipt eviction.
use super::{AcknowledgementStage, InputDisposition};
use crate::input::ResultEvent;
use fr_core::input_sequence::InputOutcome;
use fr_wire::input_result::{InputResult, Reason, ResultBinding, SequenceSpace, Stage};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_ACTIONS: u8 = 2;
pub const RETAINED_REPORTS: usize = 8;
pub const MAX_REQUEST_MICROS: u64 = 10_000_000;

/// Public comparison identity, never the host's lease, ticket or a credential.
/// A replacement foreground process, connection attempt or grant needs a NEW
/// scope and ledger. Old requests must carry their original scope unchanged.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub connection: [u8; 16],
    pub attempt: u8,
    pub lease_generation: u64,
}
impl Scope {
    fn valid(self) -> bool {
        self.connection != [0; 16] && (1..=32).contains(&self.attempt) && self.lease_generation != 0
    }
}
impl fmt::Debug for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope").finish_non_exhaustive()
    }
}

/// The digest covers the COMPLETE validated action and its preconditions. A
/// changed payload or validity window cannot reuse an already consumed ID.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RequestKey {
    pub id: u64,
    pub fingerprint: [u8; 32],
}
impl fmt::Debug for RequestKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    SubmittedToOs,
    AppliedLocally,
    RejectedBeforeSubmission,
    ExpiredBeforeSubmission,
    CancelledBeforeSubmission,
    PartiallySubmittedToOs,
    EffectUnknown,
}
impl From<InputOutcome> for Outcome {
    fn from(value: InputOutcome) -> Self {
        match value {
            InputOutcome::SubmittedToOs => Self::SubmittedToOs,
            InputOutcome::AppliedLocally => Self::AppliedLocally,
            InputOutcome::RejectedBeforeSubmission => Self::RejectedBeforeSubmission,
            InputOutcome::ExpiredBeforeSubmission => Self::ExpiredBeforeSubmission,
            InputOutcome::CancelledBeforeSubmission => Self::CancelledBeforeSubmission,
            InputOutcome::PartiallySubmittedToOs => Self::PartiallySubmittedToOs,
            InputOutcome::EffectUnknown => Self::EffectUnknown,
        }
    }
}

/// Why the request stopped progressing. A local timeout/stop never erases an
/// authenticated receipt that arrives later; its actual effects remain below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum End {
    HostResult,
    Refused,
    Busy,
    Expired,
    Stopped,
    ClockRegression,
}

/// Exact confirmed prefixes and uncertainty, without coordinates, keys, text,
/// fingerprints or host authority material. No semantic application result is
/// inferred from successful OS submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub request_id: u64,
    pub actions_total: u8,
    pub actions_encoded: u8,
    pub actions_submitted: u8,
    pub acknowledged_actions: u8,
    /// Encoded actions whose ENTIRE external outcome remains unreported.
    pub unacknowledged_actions: u8,
    pub submitted_operations: u32,
    pub stage: Option<AcknowledgementStage>,
    pub disposition: InputDisposition,
    pub host_outcome: Option<Outcome>,
    pub host_reason: Option<u16>,
    /// The host specifically reported an unknown operation after its prefix.
    /// A wholly missing receipt is counted separately above.
    pub unknown_next_operation: bool,
    pub end: End,
}

impl Report {
    /// Check an untrusted bounded IPC reply before interpreting it as success.
    /// This checks coherence only; the caller still pins the original owner,
    /// request ID and action count. It never authenticates host authority.
    pub fn validate(self) -> Result<(), Error> {
        if self.request_id == 0
            || !(1..=MAX_ACTIONS).contains(&self.actions_total)
            || self.actions_encoded > self.actions_total
            || self.actions_submitted > self.acknowledged_actions
            || u16::from(self.acknowledged_actions) + u16::from(self.unacknowledged_actions)
                != u16::from(self.actions_encoded)
            || (self.host_outcome.is_some() != (self.acknowledged_actions != 0))
            || self.unknown_next_operation != (self.host_outcome == Some(Outcome::EffectUnknown))
        {
            return Err(Error::InvalidReport);
        }
        let failed = self
            .host_outcome
            .is_some_and(|o| o != Outcome::SubmittedToOs);
        if self.actions_submitted != self.acknowledged_actions - u8::from(failed)
            || (failed && self.unacknowledged_actions != 0 && self.end != End::Stopped)
        {
            return Err(Error::InvalidReport);
        }
        // Every preceding receipt is successful. Only the final receipt can
        // contain a failed/unknown prefix; it cannot use the full per-action
        // bound while also claiming one further partial/unknown operation.
        let maximum = u32::try_from(fr_core::input::MAX_COMMITTED_TEXT_BYTES)
            .map_err(|_| Error::InvalidReport)?;
        let partial = self.host_outcome == Some(Outcome::PartiallySubmittedToOs);
        let prefix = partial || self.unknown_next_operation;
        let minimum = u32::from(self.actions_submitted) + u32::from(partial);
        let limit =
            u32::from(self.actions_submitted) * maximum + if prefix { maximum - 1 } else { 0 };
        if !(minimum..=limit).contains(&self.submitted_operations) || !self.valid_reason() {
            return Err(Error::InvalidReport);
        }
        let valid_stage = if self.actions_encoded == 0 {
            self.stage.is_none()
        } else if self.submitted_operations == 0 {
            self.stage == Some(AcknowledgementStage::Admitted)
        } else {
            matches!(
                self.stage,
                Some(AcknowledgementStage::SubmittedToOs | AcknowledgementStage::Observed)
            ) && !(self.stage == Some(AcknowledgementStage::Observed)
                && (partial || self.actions_submitted == 0))
        };
        let disposition = if self.unknown_next_operation || self.unacknowledged_actions != 0 {
            InputDisposition::UnknownEffect
        } else if self.actions_submitted == self.actions_total {
            InputDisposition::Committed
        } else if self.submitted_operations != 0 {
            InputDisposition::Partial
        } else {
            InputDisposition::Refused
        };
        if !valid_stage
            || self.disposition != disposition
            || (self.end == End::Busy && self.actions_encoded != 0)
            || (self.end == End::HostResult
                && (self.acknowledged_actions == 0
                    || self.unacknowledged_actions != 0
                    || (!failed && self.actions_submitted != self.actions_total)))
        {
            return Err(Error::InvalidReport);
        }
        Ok(())
    }

    fn valid_reason(self) -> bool {
        let Some(reason) = self.host_reason else {
            return matches!(
                self.host_outcome,
                None | Some(Outcome::SubmittedToOs | Outcome::AppliedLocally)
            );
        };
        // The v0 wire refusal codes are a contiguous, closed numeric range.
        if !(Reason::InvalidState as u16..=Reason::AuthorityUnavailable as u16).contains(&reason) {
            return false;
        }
        let expiry = [
            Reason::ObservationExpired,
            Reason::LeaseExpired,
            Reason::TicketExpired,
        ]
        .into_iter()
        .any(|r| reason == r as u16);
        match self.host_outcome {
            Some(Outcome::RejectedBeforeSubmission) => {
                !expiry
                    && reason != Reason::Revoked as u16
                    && reason != Reason::UnknownEffect as u16
            }
            Some(Outcome::ExpiredBeforeSubmission) => expiry,
            Some(Outcome::CancelledBeforeSubmission) => reason == Reason::Revoked as u16,
            Some(Outcome::PartiallySubmittedToOs) => reason != Reason::UnknownEffect as u16,
            Some(Outcome::EffectUnknown) => reason == Reason::UnknownEffect as u16,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Started,
    Pending,
    Retained(Report),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidScope,
    WrongScope,
    InvalidRequest,
    RequestChanged,
    Consumed,
    Closed,
    NoPending,
    ActionPending,
    InvalidSequence,
    InvalidReceipt,
    InvalidReport,
    ConflictingReceipt,
    ClockRegression,
}

#[derive(Clone, Copy)]
struct Action {
    binding: ResultBinding,
    sequence: u64,
    receipt: Option<InputResult>,
}
#[derive(Clone, Copy)]
struct Request {
    key: RequestKey,
    until_us: u64,
    total: u8,
    actions: [Option<Action>; MAX_ACTIONS as usize],
    encoded: u8,
    end: End,
}
impl Request {
    fn report(&self) -> Report {
        let mut report = Report {
            request_id: self.key.id,
            actions_total: self.total,
            actions_encoded: self.encoded,
            actions_submitted: 0,
            acknowledged_actions: 0,
            unacknowledged_actions: 0,
            submitted_operations: 0,
            stage: (self.encoded != 0).then_some(AcknowledgementStage::Admitted),
            disposition: InputDisposition::Refused,
            host_outcome: None,
            host_reason: None,
            unknown_next_operation: false,
            end: self.end,
        };
        let mut observed = true;
        for action in self.actions.iter().flatten() {
            if let Some(receipt) = action.receipt {
                report.acknowledged_actions += 1;
                report.actions_submitted +=
                    u8::from(receipt.outcome == InputOutcome::SubmittedToOs);
                // Each action is validated against the wire's 4096-operation
                // bound; two bounded receipts cannot overflow this sum.
                report.submitted_operations += receipt.submitted_operations;
                report.host_outcome = Some(receipt.outcome.into());
                report.host_reason = receipt.reason.map(|reason| reason as u16);
                report.unknown_next_operation |= receipt.unknown_next_operation;
                if receipt.submitted_operations != 0 {
                    observed &= receipt.stage == Stage::Observed;
                }
            }
        }
        // An encoder bookkeeping failure reserves a slot without accepting
        // any binding for a future receipt. Its possible effect stays unknown.
        report.unacknowledged_actions = self.encoded - report.acknowledged_actions;
        if report.submitted_operations != 0 {
            report.stage = Some(if observed {
                AcknowledgementStage::Observed
            } else {
                AcknowledgementStage::SubmittedToOs
            });
        }
        report.disposition = if report.unknown_next_operation || report.unacknowledged_actions != 0
        {
            InputDisposition::UnknownEffect
        } else if report.actions_submitted == self.total {
            InputDisposition::Committed
        } else if report.submitted_operations != 0 {
            InputDisposition::Partial
        } else {
            InputDisposition::Refused
        };
        debug_assert!(report.validate().is_ok());
        report
    }
    fn awaiting(&self) -> bool {
        self.actions
            .iter()
            .flatten()
            .any(|action| action.receipt.is_none())
    }
    fn accepts(&mut self, receipt: InputResult) -> Result<bool, Error> {
        let Some(action) = self.actions.iter_mut().flatten().find(|action| {
            action.sequence == receipt.sequence && action.binding == receipt.binding
        }) else {
            return Ok(false);
        };
        if let Some(old) = action.receipt {
            return if old == receipt {
                Ok(false)
            } else {
                Err(Error::ConflictingReceipt)
            };
        }
        valid_receipt(receipt)?;
        action.receipt = Some(receipt);
        Ok(true)
    }
}

/// One pending request plus eight retained fixed-size outcomes. Not Clone: two
/// copies of a live ledger must never admit the same request independently.
pub struct Ledger {
    scope: Scope,
    consumed: u64,
    previous_time: Option<u64>,
    last_encoding: Option<(ResultBinding, u64)>,
    pending: Option<Request>,
    retained: [Option<Request>; RETAINED_REPORTS],
    cursor: usize,
    closed: bool,
}
impl Ledger {
    pub fn new(scope: Scope) -> Result<Self, Error> {
        if !scope.valid() {
            return Err(Error::InvalidScope);
        }
        Ok(Self {
            scope,
            consumed: 0,
            previous_time: None,
            last_encoding: None,
            pending: None,
            retained: [None; RETAINED_REPORTS],
            cursor: 0,
            closed: false,
        })
    }
    pub const fn scope(&self) -> Scope {
        self.scope
    }
    pub const fn is_closed(&self) -> bool {
        self.closed
    }
    pub const fn consumed_through(&self) -> u64 {
        self.consumed
    }

    /// Read only. A replay returns retained evidence without renewing its
    /// deadline; an evicted old request stays consumed forever in this scope.
    pub fn lookup(&self, scope: Scope, key: RequestKey) -> Result<Option<Admission>, Error> {
        if scope != self.scope {
            return Err(Error::WrongScope);
        }
        if key.id == 0 {
            return Err(Error::InvalidRequest);
        }
        if let Some(request) = self.pending.as_ref().filter(|r| r.key.id == key.id) {
            return if request.key == key {
                Ok(Some(Admission::Pending))
            } else {
                Err(Error::RequestChanged)
            };
        }
        if let Some(request) = self.retained.iter().flatten().find(|r| r.key.id == key.id) {
            return if request.key == key {
                Ok(Some(Admission::Retained(request.report())))
            } else {
                Err(Error::RequestChanged)
            };
        }
        if key.id <= self.consumed {
            return Err(Error::Consumed);
        }
        Ok(None)
    }
    /// Call after authenticating the local caller and checking ALL original
    /// observation preconditions. Even an expired or busy request consumes its
    /// ID before returning a cached refusal. Retrying never refreshes validity.
    pub fn begin(
        &mut self,
        scope: Scope,
        key: RequestKey,
        now_us: u64,
        until_us: u64,
        actions_total: u8,
    ) -> Result<Admission, Error> {
        if scope != self.scope {
            return Err(Error::WrongScope);
        }
        self.expire(now_us)?;
        if let Some(known) = self.lookup(scope, key)? {
            return Ok(known);
        }
        if self.closed {
            return Err(Error::Closed);
        }
        if !(1..=MAX_ACTIONS).contains(&actions_total)
            || until_us.saturating_sub(now_us) > MAX_REQUEST_MICROS
        {
            return Err(Error::InvalidRequest);
        }
        self.consumed = key.id;
        let mut request = Request {
            key,
            until_us,
            total: actions_total,
            actions: [None; MAX_ACTIONS as usize],
            encoded: 0,
            end: End::Refused,
        };
        if now_us >= until_us || self.pending.is_some() {
            request.end = if now_us >= until_us {
                End::Expired
            } else {
                End::Busy
            };
            return Ok(Admission::Retained(self.retain(&request)));
        }
        self.pending = Some(request);
        Ok(Admission::Started)
    }
    /// Recheck immediately BEFORE the owner calls its original action encoder.
    /// No waiting for readiness, newer observations, new tickets or a new lease.
    pub fn ready(&mut self, id: u64, now_us: u64) -> Result<bool, Error> {
        self.pending_id(id)?;
        if self.expire(now_us)?.is_some() {
            return Ok(false);
        }
        if self.closed {
            return Err(Error::Closed);
        }
        Ok(!self.pending_id(id)?.awaiting())
    }
    /// Record the successful ORIGINAL encoder result before yielding. Call
    /// immediately after `ready` and the owner encoder, without yielding or
    /// changing requests. Sending is already possible: a bookkeeping mismatch
    /// consumes an unresolved slot, retains uncertainty and closes this owner.
    /// Each next action waits for its preceding successful receipt.
    pub fn encoded(&mut self, id: u64, binding: ResultBinding, sequence: u64) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        let previous_encoding = self.last_encoding;
        let request = self.pending_id_mut(id)?;
        let error = if request.awaiting() {
            Some(Error::ActionPending)
        } else if request.encoded >= request.total
            || binding.channel == 0
            || binding.session.as_raw() == 0
            || binding.lease.as_raw() == 0
            || previous_encoding
                .is_some_and(|(original, previous)| original != binding || previous >= sequence)
        {
            Some(Error::InvalidSequence)
        } else {
            None
        };
        if request.encoded < request.total {
            let slot = usize::from(request.encoded);
            request.encoded += 1;
            if error.is_none() {
                request.actions[slot] = Some(Action {
                    binding,
                    sequence,
                    receipt: None,
                });
            }
        }
        if let Some(error) = error {
            self.retire_with(End::Stopped);
            return Err(error);
        }
        self.last_encoding = Some((binding, sequence));
        Ok(())
    }
    /// Only matching reliable receipts from the original bound input owner
    /// advance this request. Native pointer/unrelated receipts are ignored.
    /// Late receipts can improve retained uncertainty but never resume work.
    pub fn result(&mut self, event: ResultEvent) -> Result<Option<Report>, Error> {
        let receipt = match event {
            ResultEvent::Completed(receipt) | ResultEvent::Duplicate(receipt)
                if receipt.space == SequenceSpace::Action =>
            {
                receipt
            }
            _ => return Ok(None),
        };
        let result = self.accept_result(receipt);
        if result.is_err() {
            self.retire_with(End::Stopped);
        }
        result
    }
    fn accept_result(&mut self, receipt: InputResult) -> Result<Option<Report>, Error> {
        if let Some(request) = self.pending.as_mut()
            && request.accepts(receipt)?
        {
            let failed = receipt.outcome != InputOutcome::SubmittedToOs;
            if failed || request.encoded == request.total {
                self.closed |= failed;
                return self.finish(End::HostResult).map(Some);
            }
            return Ok(None);
        }
        for request in self.retained.iter_mut().flatten() {
            if request.accepts(receipt)? {
                return Ok(Some(request.report()));
            }
        }
        Ok(None)
    }
    /// A local rejection before encoding is definite. After any encoding,
    /// preserve confirmed prefixes and unresolved effects and fence new work.
    pub fn reject(&mut self, id: u64) -> Result<Report, Error> {
        self.pending_id(id)?;
        self.finish(End::Refused)
    }
    pub fn expire(&mut self, now_us: u64) -> Result<Option<Report>, Error> {
        self.check_clock(now_us)?;
        if self.pending.as_ref().is_some_and(|r| now_us >= r.until_us) {
            return self.finish(End::Expired).map(Some);
        }
        Ok(None)
    }
    /// Terminal cleanup decision: this instance can never acquire another grant.
    pub fn retire(&mut self) -> Option<Report> {
        self.retire_with(End::Stopped)
    }
    fn retire_with(&mut self, end: End) -> Option<Report> {
        self.closed = true;
        self.pending.take().map(|mut request| {
            request.end = end;
            self.retain(&request)
        })
    }
    fn check_clock(&mut self, now_us: u64) -> Result<(), Error> {
        if self.previous_time.is_some_and(|previous| now_us < previous) {
            self.retire_with(End::ClockRegression);
            return Err(Error::ClockRegression);
        }
        self.previous_time = Some(now_us);
        Ok(())
    }
    fn pending_id(&self, id: u64) -> Result<&Request, Error> {
        self.pending
            .as_ref()
            .filter(|r| r.key.id == id)
            .ok_or(Error::NoPending)
    }
    fn pending_id_mut(&mut self, id: u64) -> Result<&mut Request, Error> {
        self.pending
            .as_mut()
            .filter(|r| r.key.id == id)
            .ok_or(Error::NoPending)
    }
    fn finish(&mut self, end: End) -> Result<Report, Error> {
        let mut request = self.pending.take().ok_or(Error::NoPending)?;
        request.end = end;
        let report = request.report();
        self.closed |= matches!(
            report.disposition,
            InputDisposition::UnknownEffect | InputDisposition::Partial
        );
        Ok(self.retain(&request))
    }
    fn retain(&mut self, request: &Request) -> Report {
        self.retained[self.cursor] = Some(*request);
        self.cursor = (self.cursor + 1) % RETAINED_REPORTS;
        request.report()
    }
}

fn valid_receipt(receipt: InputResult) -> Result<(), Error> {
    // Reuse the canonical bounded grammar instead of maintaining weaker local
    // stage/outcome/reason rules. These metadata bytes are never transmitted.
    fr_wire::input_result::encode_input_result(
        receipt,
        &mut [0; fr_wire::input_result::INPUT_RESULT_BYTES],
        &fr_core::limits::ProtocolLimits::ABSOLUTE,
        fr_wire::input::InputDirection::HostToViewer,
        fr_wire::input::InputDelivery::Reliable,
    )
    .map(|_| ())
    .map_err(|_| Error::InvalidReceipt)
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;
