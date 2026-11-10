//! Content-free input effects retained across native viewer completion.
//! These are validated host receipts, not proof an application displayed text.
#![forbid(unsafe_code)]

use fr_client::input::ResultEvent;
use fr_core::input_sequence::InputOutcome;
use fr_wire::input_result::InputResult;

/// Fixed-size totals for original receipts. Duplicates and missing retained
/// receipts never add another OS effect or invent a zero-effect result.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub completed: u64,
    pub pointer_reports: u64,
    pub submitted_action_operations: u64,
    pub applied_locally: u64,
    pub rejected: u64,
    pub expired: u64,
    pub cancelled: u64,
    pub partial: u64,
    pub unknown_next_operation: u64,
    pub duplicates: u64,
    pub unretained: u64,
}

impl Summary {
    /// Observe an event validated by the original input client. Only Completed
    /// actions have passed its duplicate ledger. Pointer reports may repeat;
    /// count them separately without adding effects or terminal action outcomes.
    /// True means one newly reported terminal action.
    pub fn observe(&mut self, event: ResultEvent) -> bool {
        let receipt = match event {
            ResultEvent::Completed(receipt) => {
                self.completed = self.completed.saturating_add(1);
                receipt
            }
            ResultEvent::Pointer(_) => {
                self.pointer_reports = self.pointer_reports.saturating_add(1);
                return false;
            }
            ResultEvent::Duplicate(_) => {
                self.duplicates = self.duplicates.saturating_add(1);
                return false;
            }
            ResultEvent::Unretained => {
                self.unretained = self.unretained.saturating_add(1);
                return false;
            }
        };
        self.receipt(receipt);
        true
    }

    fn receipt(&mut self, receipt: InputResult) {
        // A later partial/unknown outcome or cleanup cannot subtract the
        // confirmed prefix already submitted to the OS.
        self.submitted_action_operations = self
            .submitted_action_operations
            .saturating_add(u64::from(receipt.submitted_operations));
        let count = match receipt.outcome {
            InputOutcome::SubmittedToOs => return,
            InputOutcome::AppliedLocally => &mut self.applied_locally,
            InputOutcome::RejectedBeforeSubmission => &mut self.rejected,
            InputOutcome::ExpiredBeforeSubmission => &mut self.expired,
            InputOutcome::CancelledBeforeSubmission => &mut self.cancelled,
            InputOutcome::PartiallySubmittedToOs => &mut self.partial,
            InputOutcome::EffectUnknown => &mut self.unknown_next_operation,
        };
        *count = count.saturating_add(1);
    }

    #[must_use]
    pub const fn observed(self) -> bool {
        self.completed != 0 || self.pointer_reports != 0 || self.duplicates != 0 || self.unretained != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::{InputLeaseId, RemoteSessionId};
    use fr_wire::input_result::{Reason, ResultBinding, SequenceSpace, Stage};

    fn receipt(outcome: InputOutcome, submitted_operations: u32) -> InputResult {
        InputResult {
            binding: ResultBinding {
                channel: 1,
                session: RemoteSessionId::from_raw(2),
                lease: InputLeaseId::from_raw(3),
            },
            sequence: 1,
            space: SequenceSpace::Action,
            stage: if submitted_operations == 0 {
                Stage::Admitted
            } else {
                Stage::SubmittedToOs
            },
            outcome,
            submitted_operations,
            unknown_next_operation: outcome == InputOutcome::EffectUnknown,
            reason: match outcome {
                InputOutcome::SubmittedToOs | InputOutcome::AppliedLocally => None,
                InputOutcome::EffectUnknown => Some(Reason::UnknownEffect),
                InputOutcome::ExpiredBeforeSubmission => Some(Reason::TicketExpired),
                InputOutcome::CancelledBeforeSubmission => Some(Reason::Revoked),
                InputOutcome::RejectedBeforeSubmission | InputOutcome::PartiallySubmittedToOs => {
                    Some(Reason::PlatformUnavailable)
                }
            },
        }
    }

    #[test]
    fn partial_and_unknown_effects_keep_the_confirmed_prefix_without_replay() {
        let mut summary = Summary::default();
        assert!(summary.observe(ResultEvent::Completed(receipt(
            InputOutcome::PartiallySubmittedToOs,
            2,
        ))));
        assert!(summary.observe(ResultEvent::Completed(receipt(
            InputOutcome::EffectUnknown,
            3,
        ))));
        assert_eq!(summary.submitted_action_operations, 5);
        assert_eq!(summary.partial, 1);
        assert_eq!(summary.unknown_next_operation, 1);
        assert_eq!(summary.completed, 2);
    }

    #[test]
    fn duplicate_receipts_do_not_count_their_irreversible_prefix_again() {
        let mut summary = Summary::default();
        let original = receipt(InputOutcome::EffectUnknown, 3);
        assert!(summary.observe(ResultEvent::Completed(original)));
        assert!(!summary.observe(ResultEvent::Duplicate(original)));
        assert!(!summary.observe(ResultEvent::Duplicate(original)));
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.submitted_action_operations, 3);
        assert_eq!(summary.unknown_next_operation, 1);
        assert_eq!(summary.duplicates, 2);
    }

    #[test]
    fn unavailable_history_is_not_a_zero_effect_refusal_or_success() {
        let mut summary = Summary::default();
        assert!(!summary.observe(ResultEvent::Unretained));
        assert!(summary.observed());
        assert_eq!(summary.unretained, 1);
        assert_eq!(summary.completed, 0);
        assert_eq!(summary.rejected, 0);
        assert_eq!(summary.submitted_action_operations, 0);
    }

    #[test]
    fn repeated_pointer_reports_add_no_confirmed_action_effects() {
        let mut summary = Summary::default();
        for outcome in [
            InputOutcome::AppliedLocally,
            InputOutcome::RejectedBeforeSubmission,
            InputOutcome::ExpiredBeforeSubmission,
            InputOutcome::CancelledBeforeSubmission,
        ] {
            summary.observe(ResultEvent::Completed(receipt(outcome, 0)));
        }
        let mut pointer = receipt(InputOutcome::SubmittedToOs, 1);
        pointer.space = SequenceSpace::Pointer;
        assert!(!summary.observe(ResultEvent::Pointer(pointer)));
        assert!(!summary.observe(ResultEvent::Pointer(pointer)));
        assert_eq!(summary.completed, 4);
        assert_eq!(summary.pointer_reports, 2);
        assert_eq!(summary.submitted_action_operations, 0);
        assert_eq!(summary.applied_locally, 1);
        assert_eq!(summary.rejected, 1);
        assert_eq!(summary.expired, 1);
        assert_eq!(summary.cancelled, 1);
    }

    #[test]
    fn long_lived_tallies_saturate_without_erasing_prior_effects() {
        let mut summary = Summary {
            completed: u64::MAX,
            submitted_action_operations: u64::MAX - 1,
            duplicates: u64::MAX,
            ..Summary::default()
        };
        let original = receipt(InputOutcome::SubmittedToOs, 2);
        summary.observe(ResultEvent::Completed(original));
        summary.observe(ResultEvent::Duplicate(original));
        assert_eq!(summary.completed, u64::MAX);
        assert_eq!(summary.submitted_action_operations, u64::MAX);
        assert_eq!(summary.duplicates, u64::MAX);
    }
}
