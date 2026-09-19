//! Structured log events for `FrankenRemote` end-to-end sessions.
//!
//! Captures authority transitions, queue depths, per-action input dispositions,
//! recovery events, and fault injections in a machine-readable format.

use serde::{Deserialize, Serialize};

/// Source component that emitted the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Host,
    Client,
    MediaWorker,
    SessionAgent,
    Harness,
}

/// Lifecycle stages of an input action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputStage {
    /// Admitted by `FrankenRemote` client/broker into the pipeline.
    Admitted,
    /// Submitted to the OS input injection API.
    Submitted,
    /// Observed through OS feedback or echo.
    Observed,
    /// Expired before submission due to ticket TTL expiry.
    Expired,
    /// Refused due to lack of authority, stale generation, or permission loss.
    Refused,
}

/// Authority state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityState {
    Idle,
    Observing,
    ControlRequested,
    Controlling,
    Suspended,
    Revoking,
    Closed,
}

/// Typed event payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    /// Transition between authority states.
    AuthorityTransition {
        from: AuthorityState,
        to: AuthorityState,
        generation: u64,
        reason: String,
    },
    /// Queue depth snapshot for bounded-queue compliance checking.
    QueueDepth {
        queue_name: String,
        item_count: usize,
        byte_count: usize,
        capacity_bytes: usize,
    },
    /// Disposition of an input event.
    InputDisposition {
        action_id: u64,
        sequence: u64,
        stage: InputStage,
        generation: u64,
        ticket_id: u64,
        detail: Option<String>,
    },
    /// Loss recovery or reference repair event.
    RecoveryEvent {
        reason: String,
        generation: u64,
        frame_id: Option<u64>,
        recovered: bool,
    },
    /// Control revocation event.
    ControlRevoked {
        immediate: bool,
        lease_id: u64,
        os_cleanup_completed: bool,
        generation: u64,
    },
    /// Fault injected by the test harness.
    FaultInjected {
        fault_type: String,
        target_role: Option<String>,
        details: String,
    },
    /// Connection state transition.
    ConnectionStateChanged {
        from: String,
        to: String,
        attempt: u32,
    },
    /// Harness step marker.
    HarnessStep {
        step_index: usize,
        step_name: String,
        status: String,
    },
}

/// A structured log event with timestamp, source, and typed payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredLogEvent {
    /// Monotonic timestamp in nanoseconds.
    pub timestamp_ns: u64,
    /// Submitting component.
    pub source: EventSource,
    /// Typed event body.
    pub event: EventKind,
}

impl StructuredLogEvent {
    /// Create a new event with an explicit timestamp.
    #[must_use]
    pub const fn new(timestamp_ns: u64, source: EventSource, event: EventKind) -> Self {
        Self {
            timestamp_ns,
            source,
            event,
        }
    }

    /// Serialize the event to a single JSON line.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Deserialize an event from a single JSON line.
    pub fn from_json_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_json_roundtrip() {
        let event = StructuredLogEvent::new(
            1_000_000,
            EventSource::Host,
            EventKind::AuthorityTransition {
                from: AuthorityState::Idle,
                to: AuthorityState::Observing,
                generation: 1,
                reason: "viewer_admitted".to_string(),
            },
        );

        let json = event.to_json_line().unwrap();
        let decoded = StructuredLogEvent::from_json_line(&json).unwrap();
        assert_eq!(event, decoded);
    }

    #[test]
    fn input_disposition_roundtrip() {
        let event = StructuredLogEvent::new(
            2_000_000,
            EventSource::SessionAgent,
            EventKind::InputDisposition {
                action_id: 42,
                sequence: 1,
                stage: InputStage::Submitted,
                generation: 2,
                ticket_id: 100,
                detail: Some("key_press".to_string()),
            },
        );

        let json = event.to_json_line().unwrap();
        let decoded = StructuredLogEvent::from_json_line(&json).unwrap();
        assert_eq!(event, decoded);
    }
}
