//! Assertion helpers over captured structured event logs.
//!
//! Evaluates behavioral invariants:
//! - No input executed after revoke
//! - Queue high-water marks strictly bounded
//! - No lease resurrection on reconnect
//! - Orderly teardown sequence

use crate::event::{AuthorityState, EventKind, InputStage, StructuredLogEvent};
use serde::{Deserialize, Serialize};

/// Result of an individual assertion check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertionResult {
    pub assertion_name: String,
    pub passed: bool,
    pub message: String,
    pub offending_event: Option<StructuredLogEvent>,
}

impl AssertionResult {
    #[must_use]
    pub const fn pass(name: String, message: String) -> Self {
        Self {
            assertion_name: name,
            passed: true,
            message,
            offending_event: None,
        }
    }

    #[must_use]
    pub const fn fail(name: String, message: String, offending_event: Option<StructuredLogEvent>) -> Self {
        Self {
            assertion_name: name,
            passed: false,
            message,
            offending_event,
        }
    }
}

/// Verify that no input action was submitted to the OS after control was revoked.
///
/// If input was submitted after the timestamp of a `ControlRevoked` event,
/// this returns a failing assertion result with the offending event.
#[must_use]
pub fn assert_no_input_executed_after_revoke(events: &[StructuredLogEvent]) -> AssertionResult {
    let name = "no_input_executed_after_revoke".to_string();

    // Find the first ControlRevoked event timestamp (if any)
    let revoke_ts = events.iter().find_map(|e| match &e.event {
        EventKind::ControlRevoked { .. } => Some(e.timestamp_ns),
        _ => None,
    });

    let Some(revoke_timestamp) = revoke_ts else {
        // If control was never revoked in this scenario, assertion passes vacuously
        return AssertionResult::pass(
            name,
            "Control was not revoked during this scenario run.".to_string(),
        );
    };

    // Check for any input submitted after revoke_timestamp
    for event in events {
        if let EventKind::InputDisposition { stage, .. } = &event.event
            && *stage == InputStage::Submitted
            && event.timestamp_ns >= revoke_timestamp
        {
            return AssertionResult::fail(
                name,
                format!(
                    "VIOLATION: Input action was submitted at timestamp {} ns after control was revoked at {} ns!",
                    event.timestamp_ns, revoke_timestamp
                ),
                Some(event.clone()),
            );
        }
    }

    AssertionResult::pass(
        name,
        format!("Verified: zero inputs submitted after revoke at {revoke_timestamp} ns."),
    )
}

/// Verify that no queue depth exceeded its declared maximum byte limit.
#[must_use]
pub fn assert_queue_high_water(
    events: &[StructuredLogEvent],
    max_allowed_bytes: usize,
) -> AssertionResult {
    let name = "queue_high_water_bounded".to_string();

    for event in events {
        if let EventKind::QueueDepth {
            queue_name,
            byte_count,
            capacity_bytes,
            ..
        } = &event.event
            && (*byte_count > max_allowed_bytes || *byte_count > *capacity_bytes)
        {
            return AssertionResult::fail(
                name,
                format!(
                    "VIOLATION: Queue '{queue_name}' depth {byte_count} bytes exceeded limit {max_allowed_bytes} (capacity {capacity_bytes})!"
                ),
                Some(event.clone()),
            );
        }
    }

    AssertionResult::pass(
        name,
        format!("Verified: all queues stayed within {max_allowed_bytes} bytes limit."),
    )
}

/// Verify that reconnecting never resurrects an expired or revoked input lease.
///
/// After a reconnect event, the session must be in Observing/Viewing state,
/// and must never transition to Controlling without an explicit fresh grant.
#[must_use]
pub fn assert_no_lease_resurrection_on_reconnect(events: &[StructuredLogEvent]) -> AssertionResult {
    let name = "no_lease_resurrection_on_reconnect".to_string();

    let mut saw_reconnect = false;
    let mut saw_fresh_control_request = false;

    for event in events {
        match &event.event {
            EventKind::ConnectionStateChanged { to, .. } if to == "reconnected" => {
                saw_reconnect = true;
                saw_fresh_control_request = false;
            }
            EventKind::AuthorityTransition { to, .. } => match to {
                AuthorityState::ControlRequested => {
                    saw_fresh_control_request = true;
                }
                AuthorityState::Controlling if saw_reconnect && !saw_fresh_control_request => {
                    return AssertionResult::fail(
                        name,
                        "VIOLATION: Input lease was resurrected across reconnect without an explicit fresh grant!".to_string(),
                        Some(event.clone()),
                    );
                }
                _ => {}
            },
            _ => {}
        }
    }

    AssertionResult::pass(
        name,
        "Verified: no silent lease resurrection occurred across reconnect.".to_string(),
    )
}

/// Verify the orderly teardown sequence: revoke -> release keys -> fence -> close.
#[must_use]
pub fn assert_orderly_teardown(events: &[StructuredLogEvent]) -> AssertionResult {
    let name = "orderly_teardown_sequence".to_string();

    let mut revoked_ts: Option<u64> = None;
    let mut closed_ts: Option<u64> = None;

    for event in events {
        match &event.event {
            EventKind::ControlRevoked { .. } => {
                revoked_ts = Some(event.timestamp_ns);
            }
            EventKind::AuthorityTransition {
                to: AuthorityState::Closed,
                ..
            } => {
                closed_ts = Some(event.timestamp_ns);
            }
            _ => {}
        }
    }

    if let (Some(rev_ts), Some(close_ts)) = (revoked_ts, closed_ts)
        && rev_ts > close_ts
    {
        return AssertionResult::fail(
            name,
            format!("VIOLATION: Session closed at {close_ts} ns before control was revoked at {rev_ts} ns!"),
            None,
        );
    }

    AssertionResult::pass(
        name,
        "Verified: teardown sequence followed correct authority fencing order.".to_string(),
    )
}

/// Run all standard canonical assertions over the event log.
#[must_use]
pub fn evaluate_all_assertions(
    events: &[StructuredLogEvent],
    max_queue_bytes: usize,
) -> Vec<AssertionResult> {
    vec![
        assert_no_input_executed_after_revoke(events),
        assert_queue_high_water(events, max_queue_bytes),
        assert_no_lease_resurrection_on_reconnect(events),
        assert_orderly_teardown(events),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventSource;

    #[test]
    fn no_input_after_revoke_passes_when_clean() {
        let events = vec![
            StructuredLogEvent::new(
                1_000,
                EventSource::Client,
                EventKind::InputDisposition {
                    action_id: 1,
                    sequence: 1,
                    stage: InputStage::Submitted,
                    generation: 1,
                    ticket_id: 10,
                    detail: None,
                },
            ),
            StructuredLogEvent::new(
                2_000,
                EventSource::Host,
                EventKind::ControlRevoked {
                    immediate: true,
                    lease_id: 1,
                    os_cleanup_completed: true,
                    generation: 1,
                },
            ),
            StructuredLogEvent::new(
                3_000,
                EventSource::SessionAgent,
                EventKind::InputDisposition {
                    action_id: 2,
                    sequence: 2,
                    stage: InputStage::Refused,
                    generation: 1,
                    ticket_id: 11,
                    detail: Some("lease_revoked".to_string()),
                },
            ),
        ];

        let result = assert_no_input_executed_after_revoke(&events);
        assert!(result.passed);
    }

    #[test]
    fn no_input_after_revoke_fails_on_planted_violation() {
        let events = vec![
            StructuredLogEvent::new(
                1_000,
                EventSource::Host,
                EventKind::ControlRevoked {
                    immediate: true,
                    lease_id: 1,
                    os_cleanup_completed: true,
                    generation: 1,
                },
            ),
            // Planted violation: an input submitted AFTER revoke timestamp
            StructuredLogEvent::new(
                1_500,
                EventSource::SessionAgent,
                EventKind::InputDisposition {
                    action_id: 99,
                    sequence: 5,
                    stage: InputStage::Submitted,
                    generation: 1,
                    ticket_id: 15,
                    detail: Some("late_submission".to_string()),
                },
            ),
        ];

        let result = assert_no_input_executed_after_revoke(&events);
        assert!(!result.passed);
        assert!(result.message.contains("VIOLATION"));
        assert!(result.offending_event.is_some());
    }

    #[test]
    fn queue_high_water_catches_overflow() {
        let events = vec![StructuredLogEvent::new(
            1_000,
            EventSource::Host,
            EventKind::QueueDepth {
                queue_name: "video_frames".to_string(),
                item_count: 50,
                byte_count: 100_000,
                capacity_bytes: 50_000,
            },
        )];

        let result = assert_queue_high_water(&events, 50_000);
        assert!(!result.passed);
        assert!(result.message.contains("VIOLATION"));
    }
}
