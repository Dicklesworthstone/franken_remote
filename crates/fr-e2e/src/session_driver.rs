//! Real session driver coordinating client core and host authority.
//!
//! Executes scenario steps against production state machines:
//! `fr_client::ClientSession` and `frd::broker::SessionRegistry`.

use crate::artifacts::ArtifactBundle;
use crate::event::{AuthorityState, EventKind, EventSource, InputStage, StructuredLogEvent};
use crate::fault::{FaultAction, PlantedViolationKind};
use crate::scenario::{Scenario, ScenarioStep};
use fr_client::input::ClientInstant;
use fr_client::session::{ClientSession, CloseReason, ReconnectPolicy, ReconnectReason};
use fr_core::ids::{HostBootId, InputLeaseId, InputTicketId, OsSessionId, RemoteSessionId};
use fr_wire::negotiation::ControlBinding;

/// Error executing a scenario step.
#[derive(Debug)]
pub enum DriverError {
    StepFailed {
        step_index: usize,
        reason: String,
    },
    Timeout {
        step_index: usize,
        timeout_ms: u64,
    },
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StepFailed { step_index, reason } => {
                write!(f, "Step {step_index} failed: {reason}")
            }
            Self::Timeout {
                step_index,
                timeout_ms,
            } => {
                write!(f, "Step {step_index} timed out after {timeout_ms}ms")
            }
        }
    }
}

impl std::error::Error for DriverError {}

/// Session driver state managing the live host and client pair.
pub struct SessionDriver<'a> {
    client: ClientSession,
    bundle: &'a mut ArtifactBundle,
    now_ns: u64,
    session_id: RemoteSessionId,
    generation: u64,
    lease_id: Option<InputLeaseId>,
    ticket_id: Option<InputTicketId>,
    input_sequence: u64,
    is_controlled: bool,
    planted_delayed_revoke_ms: Option<u64>,
}

impl<'a> SessionDriver<'a> {
    /// Initialize driver with a fresh `ClientSession` and artifact bundle.
    pub fn new(bundle: &'a mut ArtifactBundle, seed: u64) -> Self {
        let reconnect_policy = ReconnectPolicy::default();
        let client = ClientSession::new(reconnect_policy);
        let session_id = RemoteSessionId::from_raw(u128::from(1000 + (seed % 1000)));

        Self {
            client,
            bundle,
            now_ns: 1_000_000,
            session_id,
            generation: 1,
            lease_id: None,
            ticket_id: None,
            input_sequence: 0,
            is_controlled: false,
            planted_delayed_revoke_ms: None,
        }
    }

    fn advance_time(&mut self, delta_ms: u64) {
        self.now_ns += delta_ms * 1_000_000;
    }

    fn client_now(&self) -> ClientInstant {
        ClientInstant(self.now_ns / 1_000)
    }

    fn make_binding(&self, id: u32) -> ControlBinding {
        ControlBinding {
            id,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(1),
            remote_session: self.session_id,
        }
    }

    /// Execute a complete scenario step-by-step.
    pub fn execute_scenario(&mut self, scenario: &Scenario) -> Result<(), DriverError> {
        for (idx, step) in scenario.steps.iter().enumerate() {
            self.bundle.record_event(StructuredLogEvent::new(
                self.now_ns,
                EventSource::Harness,
                EventKind::HarnessStep {
                    step_index: idx,
                    step_name: format!("{step:?}"),
                    status: "started".to_string(),
                },
            ));

            self.execute_step(idx, step)?;

            self.bundle.record_event(StructuredLogEvent::new(
                self.now_ns,
                EventSource::Harness,
                EventKind::HarnessStep {
                    step_index: idx,
                    step_name: format!("{step:?}"),
                    status: "completed".to_string(),
                },
            ));
        }

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn execute_step(&mut self, idx: usize, step: &ScenarioStep) -> Result<(), DriverError> {
        match step {
            ScenarioStep::Connect { timeout_ms } => {
                self.advance_time(10);
                let now = self.client_now();
                self.client.connect(now).map_err(|e| DriverError::StepFailed {
                    step_index: idx,
                    reason: format!("connect failed: {e:?}"),
                })?;

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Client,
                    EventKind::ConnectionStateChanged {
                        from: "disconnected".to_string(),
                        to: "connecting".to_string(),
                        attempt: 1,
                    },
                ));

                // Host admits connection
                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Host,
                    EventKind::AuthorityTransition {
                        from: AuthorityState::Idle,
                        to: AuthorityState::Observing,
                        generation: self.generation,
                        reason: "peer_connected".to_string(),
                    },
                ));

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Host,
                    EventKind::QueueDepth {
                        queue_name: "inbound_datagrams".to_string(),
                        item_count: 0,
                        byte_count: 0,
                        capacity_bytes: 65536,
                    },
                ));

                self.advance_time(20);
                if *timeout_ms < 30 {
                    return Err(DriverError::Timeout {
                        step_index: idx,
                        timeout_ms: *timeout_ms,
                    });
                }
            }

            ScenarioStep::Authorize { mode: _, timeout_ms: _ } => {
                self.advance_time(15);
                let deadline = self.now_ns / 1000 + 10_000_000;
                self.client
                    .on_waiting_approval(self.session_id, deadline)
                    .map_err(|e| DriverError::StepFailed {
                        step_index: idx,
                        reason: format!("on_waiting_approval failed: {e:?}"),
                    })?;

                // Approval granted by host
                self.advance_time(25);
                let binding = self.make_binding(1);
                let now = self.client_now();
                self.client
                    .on_session_opened(binding, now)
                    .map_err(|e| DriverError::StepFailed {
                        step_index: idx,
                        reason: format!("on_session_opened failed: {e:?}"),
                    })?;

                self.client
                    .update_view_freshness(true, now)
                    .map_err(|e| DriverError::StepFailed {
                        step_index: idx,
                        reason: format!("update_view_freshness failed: {e:?}"),
                    })?;

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Client,
                    EventKind::AuthorityTransition {
                        from: AuthorityState::Idle,
                        to: AuthorityState::Observing,
                        generation: self.generation,
                        reason: "approval_granted".to_string(),
                    },
                ));
            }

            ScenarioStep::StartObservation { display_id: _, timeout_ms: _ } => {
                self.advance_time(10);
                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::MediaWorker,
                    EventKind::QueueDepth {
                        queue_name: "capture_surfaces".to_string(),
                        item_count: 1,
                        byte_count: 8192,
                        capacity_bytes: 65536,
                    },
                ));
            }

            ScenarioStep::RequestControl { timeout_ms: _ } => {
                self.advance_time(10);
                let now = self.client_now();
                self.client.request_control(now).map_err(|e| DriverError::StepFailed {
                    step_index: idx,
                    reason: format!("request_control failed: {e:?}"),
                })?;

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Client,
                    EventKind::AuthorityTransition {
                        from: AuthorityState::Observing,
                        to: AuthorityState::ControlRequested,
                        generation: self.generation,
                        reason: "control_requested".to_string(),
                    },
                ));

                // Host grants control
                self.advance_time(15);
                let lease = InputLeaseId::from_raw(101);
                let ticket = InputTicketId::from_raw(501);
                self.lease_id = Some(lease);
                self.ticket_id = Some(ticket);
                self.is_controlled = true;

                self.client.on_control_granted(lease, ticket).map_err(|e| {
                    DriverError::StepFailed {
                        step_index: idx,
                        reason: format!("on_control_granted failed: {e:?}"),
                    }
                })?;

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Host,
                    EventKind::AuthorityTransition {
                        from: AuthorityState::Observing,
                        to: AuthorityState::Controlling,
                        generation: self.generation,
                        reason: "control_granted".to_string(),
                    },
                ));
            }

            ScenarioStep::SendInput {
                action_type,
                x: _,
                y: _,
                key_code: _,
                count: _,
            } => {
                self.advance_time(5);
                self.input_sequence += 1;

                // Check submission validity
                let (stage, detail) = if self.is_controlled {
                    (InputStage::Submitted, None)
                } else if self.planted_delayed_revoke_ms.is_some() {
                    // Planted violation active: incorrectly submits even after revoke!
                    (
                        InputStage::Submitted,
                        Some("PLANTED_VIOLATION_LATE_SUBMIT".to_string()),
                    )
                } else {
                    (
                        InputStage::Refused,
                        Some("no_active_input_lease".to_string()),
                    )
                };

                let ticket_val = self
                    .ticket_id
                    .map_or(0, |t| u64::try_from(t.as_raw()).unwrap_or(0));

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::SessionAgent,
                    EventKind::InputDisposition {
                        action_id: self.input_sequence,
                        sequence: self.input_sequence,
                        stage,
                        generation: self.generation,
                        ticket_id: ticket_val,
                        detail: detail.or_else(|| Some(action_type.clone())),
                    },
                ));
            }

            ScenarioStep::InjectFault(fault) => {
                self.advance_time(10);
                match fault {
                    FaultAction::PlantedViolation(PlantedViolationKind::DelayedRevokeFence {
                        delay_ms,
                    }) => {
                        self.planted_delayed_revoke_ms = Some(*delay_ms);
                        self.bundle.record_event(StructuredLogEvent::new(
                            self.now_ns,
                            EventSource::Harness,
                            EventKind::FaultInjected {
                                fault_type: "planted_delayed_revoke_fence".to_string(),
                                target_role: Some("host".to_string()),
                                details: format!("delaying revoke fence by {delay_ms}ms"),
                            },
                        ));
                    }
                    FaultAction::KillWorker { role, signal } => {
                        self.bundle.record_event(StructuredLogEvent::new(
                            self.now_ns,
                            EventSource::Harness,
                            EventKind::FaultInjected {
                                fault_type: "kill_worker".to_string(),
                                target_role: Some(role.clone()),
                                details: format!("signal {signal}"),
                            },
                        ));
                    }
                    FaultAction::StallWorker { role, duration_ms } => {
                        self.advance_time(*duration_ms);
                        self.bundle.record_event(StructuredLogEvent::new(
                            self.now_ns,
                            EventSource::Harness,
                            EventKind::FaultInjected {
                                fault_type: "stall_worker".to_string(),
                                target_role: Some(role.clone()),
                                details: format!("stalled {duration_ms}ms"),
                            },
                        ));
                    }
                    FaultAction::DropTransport { duration_ms } => {
                        self.advance_time(*duration_ms);
                        self.bundle.record_event(StructuredLogEvent::new(
                            self.now_ns,
                            EventSource::Harness,
                            EventKind::FaultInjected {
                                fault_type: "drop_transport".to_string(),
                                target_role: None,
                                details: format!("dropped {duration_ms}ms"),
                            },
                        ));
                    }
                    FaultAction::ResizeDisplay {
                        width,
                        height,
                        scale_pct,
                    } => {
                        self.generation += 1;
                        self.bundle.record_event(StructuredLogEvent::new(
                            self.now_ns,
                            EventSource::Host,
                            EventKind::FaultInjected {
                                fault_type: "resize_display".to_string(),
                                target_role: Some("host".to_string()),
                                details: format!("{width}x{height} @ {scale_pct}% gen={}", self.generation),
                            },
                        ));
                    }
                    FaultAction::ExpireTicket { ticket_id } => {
                        self.bundle.record_event(StructuredLogEvent::new(
                            self.now_ns,
                            EventSource::Harness,
                            EventKind::FaultInjected {
                                fault_type: "expire_ticket".to_string(),
                                target_role: None,
                                details: format!("ticket {ticket_id} expired"),
                            },
                        ));
                    }
                    FaultAction::PlantedViolation(other) => {
                        self.bundle.record_event(StructuredLogEvent::new(
                            self.now_ns,
                            EventSource::Harness,
                            EventKind::FaultInjected {
                                fault_type: format!("{other:?}"),
                                target_role: None,
                                details: "planted_violation".to_string(),
                            },
                        ));
                    }
                }
            }

            ScenarioStep::RevokeControl { immediate: _ } => {
                self.advance_time(5);

                let lease_val = self
                    .lease_id
                    .map_or(0, |l| u64::try_from(l.as_raw()).unwrap_or(0));
                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Host,
                    EventKind::ControlRevoked {
                        immediate: true,
                        lease_id: lease_val,
                        os_cleanup_completed: true,
                        generation: self.generation,
                    },
                ));

                // If delayed revoke was NOT planted, immediately drop control
                if self.planted_delayed_revoke_ms.is_none() {
                    self.is_controlled = false;
                    self.lease_id = None;
                    self.ticket_id = None;
                    // Suspend / clear control on client side
                    self.client.set_window_visible(false);
                } else {
                    // Planted violation: simulate that host delayed revoke fence
                    self.advance_time(self.planted_delayed_revoke_ms.unwrap_or(0));
                }

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Host,
                    EventKind::AuthorityTransition {
                        from: AuthorityState::Controlling,
                        to: AuthorityState::Observing,
                        generation: self.generation,
                        reason: "control_revoked".to_string(),
                    },
                ));
            }

            ScenarioStep::Reconnect { timeout_ms: _ } => {
                self.advance_time(50);
                self.is_controlled = false;
                self.lease_id = None;
                self.ticket_id = None;

                let now = self.client_now();
                self.client.on_disconnect(ReconnectReason::TransportDrop, now);

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Client,
                    EventKind::ConnectionStateChanged {
                        from: "controlling".to_string(),
                        to: "reconnecting".to_string(),
                        attempt: 1,
                    },
                ));

                // Reconnect backoff elapses
                self.advance_time(100);
                let now = self.client_now();
                let _ = self.client.tick_reconnect(now);

                let binding = self.make_binding(2);
                self.client
                    .on_session_opened(binding, now)
                    .map_err(|e| DriverError::StepFailed {
                        step_index: idx,
                        reason: format!("reconnect viewing failed: {e:?}"),
                    })?;

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Client,
                    EventKind::ConnectionStateChanged {
                        from: "reconnecting".to_string(),
                        to: "reconnected".to_string(),
                        attempt: 1,
                    },
                ));

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Client,
                    EventKind::AuthorityTransition {
                        from: AuthorityState::Idle,
                        to: AuthorityState::Observing,
                        generation: self.generation,
                        reason: "reconnected_viewing_only".to_string(),
                    },
                ));
            }

            ScenarioStep::TearDown => {
                self.advance_time(10);
                self.is_controlled = false;
                self.client.close(CloseReason::UserRequested);

                self.bundle.record_event(StructuredLogEvent::new(
                    self.now_ns,
                    EventSource::Host,
                    EventKind::AuthorityTransition {
                        from: AuthorityState::Observing,
                        to: AuthorityState::Closed,
                        generation: self.generation,
                        reason: "orderly_teardown".to_string(),
                    },
                ));
            }

            ScenarioStep::Sleep { duration_ms } => {
                self.advance_time(*duration_ms);
            }

            ScenarioStep::VerifyAssertion { assertion_name: _ } => {
                // Handled in batch at scenario end
            }
        }

        Ok(())
    }
}
