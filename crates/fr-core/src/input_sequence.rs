//! Bounded, lease-scoped accounting for irreversible input submissions.
//!
//! The consumed sequence floor is independent of the small receipt cache:
//! forgetting an old result must never make its action executable again.
//! This owner admits at most one unresolved action at a time. It records no
//! key, pointer, clipboard, text, or other user-content payload.
//!
//! Admission consumes the sequence BEFORE an OS effect can occur. The caller
//! must then recheck session authority, ticket expiry, geometry, and mapping
//! immediately before submission and record the outcome even on refusal.
//! Cancellation or uncertainty fences dependent actions; release-only cleanup
//! is a separate local operation, not a new remotely admitted action.
//!
//! This is not authentication, a wire codec, a durable exactly-once protocol,
//! or evidence that an application processed input. A reconnect needs a fresh
//! lease and a fresh ledger; it never resumes an old uncertain action.

use crate::ids::InputLeaseId;

/// Fixed implementation storage bound, not an advertised wire capability.
/// A smaller receipt window can be selected without weakening replay defense.
pub const MAX_RETAINED_INPUT_RECEIPTS: usize = 32;

/// Content-free outcome recorded after an admitted action reaches a terminal
/// submission decision. None of these means an application effect was proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    /// The requested input was submitted to the OS API.
    SubmittedToOs,
    /// Input mode/state changed without an external OS operation.
    AppliedLocally,
    /// Rejected before any OS submission.
    RejectedBeforeSubmission,
    /// Ticket/authority expired before any OS submission.
    ExpiredBeforeSubmission,
    /// Cancelled before any OS submission.
    CancelledBeforeSubmission,
    /// Some input was submitted; the full action was not.
    PartiallySubmittedToOs,
    /// External effects cannot be determined; automatic retry is forbidden.
    EffectUnknown,
}

/// Result of attempting to admit a reliable action sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum InputAdmission {
    /// Newly consumed sequence. Perform final authorization before injection.
    Admitted,
    /// This action is already unresolved. Do not submit it a second time.
    InFlight,
    /// A retained terminal result; return it without executing again.
    Completed(InputOutcome),
    /// Already consumed, but its receipt was evicted. Never re-execute it.
    ConsumedWithoutReceipt,
}

/// Refusals that do not admit a new external effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputSequenceError {
    /// Receipt capacity must be within the fixed implementation bound.
    InvalidCapacity {
        /// Requested number of retained receipts.
        requested: usize,
    },
    /// The request references a different lease.
    StaleLease,
    /// The ledger was fenced; a new lease is required for new actions.
    Fenced,
    /// A preceding action has not yet received its terminal outcome.
    PreviousActionPending {
        /// Sequence still awaiting an outcome.
        sequence: u64,
    },
    /// A gap on the ordered action stream fenced further admissions.
    SequenceGap {
        /// Sequence that was required next.
        expected: u64,
        /// Sequence actually received.
        received: u64,
    },
    /// An outcome did not identify the single pending action.
    NotPending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Receipt {
    sequence: u64,
    outcome: InputOutcome,
}

/// Single-owner ledger for one input lease. Deliberately not `Clone`: copying
/// a live admission owner could permit two independent OS submissions.
/// The fixed array bounds count and storage without allocation in the path.
#[derive(Debug)]
pub struct InputSequenceLedger {
    lease: InputLeaseId,
    next_sequence: Option<u64>,
    pending: Option<u64>,
    receipts: [Option<Receipt>; MAX_RETAINED_INPUT_RECEIPTS],
    receipt_capacity: usize,
    receipt_cursor: usize,
    fenced: bool,
}

impl InputSequenceLedger {
    /// Constructs a fresh lease epoch, starting at reliable action sequence 0.
    /// The containing authority owner must generate a fresh lease identity;
    /// callers may not reconstruct this ledger to resume an old lease.
    pub fn new(lease: InputLeaseId, receipt_capacity: usize) -> Result<Self, InputSequenceError> {
        if !(1..=MAX_RETAINED_INPUT_RECEIPTS).contains(&receipt_capacity) {
            return Err(InputSequenceError::InvalidCapacity {
                requested: receipt_capacity,
            });
        }
        Ok(Self {
            lease,
            next_sequence: Some(0),
            pending: None,
            receipts: [None; MAX_RETAINED_INPUT_RECEIPTS],
            receipt_capacity,
            receipt_cursor: 0,
            fenced: false,
        })
    }

    /// Consumes exactly the next sequence, or returns a no-replay result.
    /// Duplicate/result reporting remains possible after fencing, but only
    /// for this same lease; it never restores submission authority.
    pub fn admit(
        &mut self,
        lease: InputLeaseId,
        sequence: u64,
    ) -> Result<InputAdmission, InputSequenceError> {
        self.check_lease(lease)?;
        if self.pending == Some(sequence) {
            return Ok(InputAdmission::InFlight);
        }
        if let Some(receipt) = self.receipts[..self.receipt_capacity]
            .iter()
            .flatten()
            .find(|receipt| receipt.sequence == sequence)
        {
            return Ok(InputAdmission::Completed(receipt.outcome));
        }
        if self.next_sequence.is_none_or(|next| sequence < next) {
            return Ok(InputAdmission::ConsumedWithoutReceipt);
        }
        if self.fenced {
            return Err(InputSequenceError::Fenced);
        }
        if let Some(pending) = self.pending {
            return Err(InputSequenceError::PreviousActionPending { sequence: pending });
        }
        // The exhausted case was handled above without allowing wraparound.
        let expected = self.next_sequence.ok_or(InputSequenceError::Fenced)?;
        if sequence != expected {
            self.fenced = true;
            return Err(InputSequenceError::SequenceGap {
                expected,
                received: sequence,
            });
        }
        self.next_sequence = sequence.checked_add(1);
        self.pending = Some(sequence);
        Ok(InputAdmission::Admitted)
    }

    /// Records exactly one terminal outcome. A refusal, cancellation, partial
    /// submission, or unknown effect fences dependent actions. The consumed
    /// sequence remains retired even after its bounded receipt is evicted.
    /// Fencing while an OS call is outstanding does not fabricate its result:
    /// the real outcome may still be recorded here afterward.
    pub fn record_outcome(
        &mut self,
        lease: InputLeaseId,
        sequence: u64,
        outcome: InputOutcome,
    ) -> Result<(), InputSequenceError> {
        self.check_lease(lease)?;
        if self.pending != Some(sequence) {
            return Err(InputSequenceError::NotPending);
        }
        self.pending = None;
        self.receipts[self.receipt_cursor] = Some(Receipt { sequence, outcome });
        self.receipt_cursor = (self.receipt_cursor + 1) % self.receipt_capacity;
        if !matches!(
            outcome,
            InputOutcome::SubmittedToOs | InputOutcome::AppliedLocally
        ) {
            self.fenced = true;
        }
        Ok(())
    }

    /// Stops new admissions without pretending an outstanding effect rolled
    /// back. Local held-key cleanup remains outside this ledger.
    pub fn fence(&mut self) {
        self.fenced = true;
    }

    /// Whether new actions have been fenced explicitly or by an outcome/gap.
    #[must_use]
    pub const fn is_fenced(&self) -> bool {
        self.fenced
    }

    /// Next admissible sequence, or `None` after consuming `u64::MAX`.
    /// Exhaustion requires a new lease, never resetting this counter.
    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }

    /// The single unresolved sequence, when an action is awaiting its result.
    #[must_use]
    pub const fn pending_sequence(&self) -> Option<u64> {
        self.pending
    }

    fn check_lease(&self, lease: InputLeaseId) -> Result<(), InputSequenceError> {
        if lease == self.lease {
            Ok(())
        } else {
            Err(InputSequenceError::StaleLease)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::{AuthorityError, AuthorityPolicy, SessionAuthority};
    use crate::ids::{InputTicketId, RemoteSessionId};
    use crate::time::HostInstant;

    fn lease() -> InputLeaseId {
        InputLeaseId::from_raw(1)
    }

    fn ledger(capacity: usize) -> InputSequenceLedger {
        InputSequenceLedger::new(lease(), capacity).unwrap()
    }

    fn submit(owner: &mut InputSequenceLedger, sequence: u64) {
        assert_eq!(owner.admit(lease(), sequence), Ok(InputAdmission::Admitted));
        owner
            .record_outcome(lease(), sequence, InputOutcome::SubmittedToOs)
            .unwrap();
    }

    #[test]
    fn receipt_capacity_is_bounded() {
        for capacity in [0, MAX_RETAINED_INPUT_RECEIPTS + 1, usize::MAX] {
            assert!(matches!(
                InputSequenceLedger::new(lease(), capacity),
                Err(InputSequenceError::InvalidCapacity { requested }) if requested == capacity
            ));
        }
        assert!(InputSequenceLedger::new(lease(), 1).is_ok());
        assert!(InputSequenceLedger::new(lease(), MAX_RETAINED_INPUT_RECEIPTS).is_ok());
    }

    #[test]
    fn duplicates_before_and_after_completion_never_readmit() {
        let mut owner = ledger(2);
        assert_eq!(owner.admit(lease(), 0), Ok(InputAdmission::Admitted));
        assert_eq!(owner.admit(lease(), 0), Ok(InputAdmission::InFlight));
        owner
            .record_outcome(lease(), 0, InputOutcome::SubmittedToOs)
            .unwrap();
        assert_eq!(
            owner.admit(lease(), 0),
            Ok(InputAdmission::Completed(InputOutcome::SubmittedToOs))
        );
        assert_eq!(owner.next_sequence(), Some(1));
    }

    #[test]
    fn receipt_eviction_never_lowers_the_consumed_floor() {
        let mut owner = ledger(1);
        for sequence in 0..100 {
            submit(&mut owner, sequence);
        }
        for sequence in 0..99 {
            assert_eq!(
                owner.admit(lease(), sequence),
                Ok(InputAdmission::ConsumedWithoutReceipt)
            );
        }
        assert_eq!(
            owner.admit(lease(), 99),
            Ok(InputAdmission::Completed(InputOutcome::SubmittedToOs))
        );
        assert_eq!(owner.next_sequence(), Some(100));
    }

    #[test]
    fn preceding_action_must_finish_before_the_next_is_admitted() {
        let mut owner = ledger(2);
        assert_eq!(owner.admit(lease(), 0), Ok(InputAdmission::Admitted));
        assert_eq!(
            owner.admit(lease(), 1),
            Err(InputSequenceError::PreviousActionPending { sequence: 0 })
        );
        assert_eq!(owner.next_sequence(), Some(1));
        owner
            .record_outcome(lease(), 0, InputOutcome::SubmittedToOs)
            .unwrap();
        submit(&mut owner, 1);
    }

    #[test]
    fn a_gap_fences_instead_of_skipping_an_unknown_action() {
        let mut owner = ledger(2);
        assert_eq!(
            owner.admit(lease(), 1),
            Err(InputSequenceError::SequenceGap {
                expected: 0,
                received: 1,
            })
        );
        assert!(owner.is_fenced());
        assert_eq!(owner.admit(lease(), 0), Err(InputSequenceError::Fenced));
        assert_eq!(owner.next_sequence(), Some(0));
    }

    #[test]
    fn foreign_lease_never_consumes_or_finishes_an_action() {
        let mut owner = ledger(2);
        let foreign = InputLeaseId::from_raw(2);
        assert_eq!(owner.admit(foreign, 0), Err(InputSequenceError::StaleLease));
        assert_eq!(owner.next_sequence(), Some(0));
        assert_eq!(owner.admit(lease(), 0), Ok(InputAdmission::Admitted));
        assert_eq!(
            owner.record_outcome(foreign, 0, InputOutcome::SubmittedToOs),
            Err(InputSequenceError::StaleLease)
        );
        assert_eq!(owner.pending_sequence(), Some(0));
    }

    #[test]
    fn wrong_or_duplicate_completion_cannot_change_a_receipt() {
        let mut owner = ledger(2);
        assert_eq!(owner.admit(lease(), 0), Ok(InputAdmission::Admitted));
        assert_eq!(
            owner.record_outcome(lease(), 1, InputOutcome::SubmittedToOs),
            Err(InputSequenceError::NotPending)
        );
        owner
            .record_outcome(lease(), 0, InputOutcome::EffectUnknown)
            .unwrap();
        assert_eq!(
            owner.record_outcome(lease(), 0, InputOutcome::SubmittedToOs),
            Err(InputSequenceError::NotPending)
        );
        assert_eq!(
            owner.admit(lease(), 0),
            Ok(InputAdmission::Completed(InputOutcome::EffectUnknown))
        );
    }

    #[test]
    fn fence_does_not_fabricate_completion_of_an_outstanding_effect() {
        let mut owner = ledger(2);
        assert_eq!(owner.admit(lease(), 0), Ok(InputAdmission::Admitted));
        owner.fence();
        assert_eq!(owner.pending_sequence(), Some(0));
        assert_eq!(owner.admit(lease(), 0), Ok(InputAdmission::InFlight));
        assert_eq!(owner.admit(lease(), 1), Err(InputSequenceError::Fenced));
        owner
            .record_outcome(lease(), 0, InputOutcome::SubmittedToOs)
            .unwrap();
        assert_eq!(
            owner.admit(lease(), 0),
            Ok(InputAdmission::Completed(InputOutcome::SubmittedToOs))
        );
        assert!(owner.is_fenced());
    }

    #[test]
    fn failed_or_uncertain_outcomes_fence_all_dependent_actions() {
        for outcome in [
            InputOutcome::RejectedBeforeSubmission,
            InputOutcome::ExpiredBeforeSubmission,
            InputOutcome::CancelledBeforeSubmission,
            InputOutcome::PartiallySubmittedToOs,
            InputOutcome::EffectUnknown,
        ] {
            let mut owner = ledger(2);
            assert_eq!(owner.admit(lease(), 0), Ok(InputAdmission::Admitted));
            owner.record_outcome(lease(), 0, outcome).unwrap();
            assert!(owner.is_fenced());
            assert_eq!(owner.admit(lease(), 1), Err(InputSequenceError::Fenced));
            assert_eq!(
                owner.admit(lease(), 0),
                Ok(InputAdmission::Completed(outcome))
            );
        }
    }

    #[test]
    fn exhausted_sequences_cannot_wrap_into_old_actions() {
        let mut owner = ledger(1);
        // Only a test can seed the floor; the public constructor always starts
        // a fresh lease at zero and exposes no counter-reset operation.
        owner.next_sequence = Some(u64::MAX);
        submit(&mut owner, u64::MAX);
        assert_eq!(owner.next_sequence(), None);
        assert_eq!(
            owner.admit(lease(), u64::MAX),
            Ok(InputAdmission::Completed(InputOutcome::SubmittedToOs))
        );
        assert_eq!(
            owner.admit(lease(), 0),
            Ok(InputAdmission::ConsumedWithoutReceipt)
        );
    }

    #[test]
    fn expiry_after_admission_is_recorded_without_a_fresh_ticket_retry() {
        let now = HostInstant::from_micros(0);
        let ticket = InputTicketId::from_raw(10);
        let mut authority = SessionAuthority::new(
            RemoteSessionId::from_raw(3),
            AuthorityPolicy::plan_defaults(),
        );
        authority.mark_capabilities_checked().unwrap();
        authority.authorize_observation(now).unwrap();
        authority.mark_view_ready(now).unwrap();
        authority.grant_lease(lease(), now).unwrap();
        let deadline = authority.issue_input_ticket(lease(), ticket, now).unwrap();
        let mut owner = ledger(2);
        assert_eq!(owner.admit(lease(), 0), Ok(InputAdmission::Admitted));
        assert_eq!(
            authority.authorize_submission(lease(), ticket, deadline),
            Err(AuthorityError::TicketExpired)
        );
        owner
            .record_outcome(lease(), 0, InputOutcome::ExpiredBeforeSubmission)
            .unwrap();
        authority
            .issue_input_ticket(lease(), InputTicketId::from_raw(11), deadline)
            .unwrap();
        assert_eq!(
            owner.admit(lease(), 0),
            Ok(InputAdmission::Completed(
                InputOutcome::ExpiredBeforeSubmission
            ))
        );
        assert_eq!(owner.admit(lease(), 1), Err(InputSequenceError::Fenced));
    }
}
