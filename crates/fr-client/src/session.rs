//! Shared client session state machine, auto-reconnect controller,
//! presentation scheduling, view freshness tracking, and input authority fencing.
//!
//! # Invariants (per PLAN sections 7.1-7.3, 11.3, 14.4, 16.1):
//! - Observation, readiness, and input authority are strictly separate state variables.
//! - A media reconnect can preserve a viewing session, but NEVER silently restores
//!   a revoked or expired input lease ("reconnect-without-lease-resurrection").
//! - Auto-reconnect uses bounded backoff and publishes a user-visible `Reconnecting` state.
//! - Reconnect attempts and outcomes are logged with structured `[StateTrace: ...]` records.
//! - Control can only be requested after view freshness is re-established.
//! - Sustained unknown or stale presentation suspends input readiness immediately,
//!   while read-only diagnostics continue.
//! - Stale display geometry or viewport mapping generations reject coordinate input.
//! - In the presentation queue, the newest ready frame wins; obsolete presentation
//!   work is discarded, while decoded reference pictures remain preserved.
//! - Background or occluded rendering does not count as presentation and suspends input.

use crate::input::ClientInstant;
use fr_core::ids::{
    DisplayGeometryGeneration, HostBootId, InputLeaseId, InputTicketId, RemoteSessionId,
    ViewportMappingGeneration,
};
use fr_wire::negotiation::ControlBinding;
use std::fmt;

/// User-visible high-level connection and session lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// Disconnected / initial idle state.
    Disconnected,
    /// Connection establishment or handshake in progress.
    Connecting {
        attempt: u32,
        started_at: ClientInstant,
    },
    /// Host local approval is pending for this observation session.
    WaitingApproval {
        request: RemoteSessionId,
        host_deadline_us: u64,
    },
    /// Viewing session is active: media frames can be received and decoded.
    /// Input control is not held.
    Viewing {
        session: RemoteSessionId,
        binding: ControlBinding,
        view_fresh: bool,
    },
    /// Control grant has been explicitly requested; awaiting host grant response.
    RequestingControl {
        session: RemoteSessionId,
        request_sequence: u64,
        requested_at: ClientInstant,
    },
    /// Active input controller: valid lease and ticket held, input submission permitted.
    Controlling {
        session: RemoteSessionId,
        lease: InputLeaseId,
        ticket: InputTicketId,
        input_ready: bool,
    },
    /// Input is suspended (e.g. stale view, window occluded/unfocused), but viewing
    /// and diagnostics remain active.
    Suspended {
        session: RemoteSessionId,
        reason: SuspendReason,
    },
    /// Connection dropped or failed; auto-reconnect is actively backing off.
    /// User-visible state per plan requirements.
    Reconnecting {
        attempt: u32,
        backoff_us: u64,
        next_retry: ClientInstant,
        reason: ReconnectReason,
    },
    /// Session closed permanently.
    Closed { reason: CloseReason },
}

impl SessionState {
    /// Human-readable label for UI indicators.
    pub fn display_label(&self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting { .. } => "Connecting",
            Self::WaitingApproval { .. } => "Waiting for Approval",
            Self::Viewing { view_fresh, .. } => {
                if *view_fresh {
                    "Viewing (Fresh)"
                } else {
                    "Viewing (Stale Source)"
                }
            }
            Self::RequestingControl { .. } => "Requesting Control",
            Self::Controlling { input_ready, .. } => {
                if *input_ready {
                    "Controlling"
                } else {
                    "Control Paused"
                }
            }
            Self::Suspended { .. } => "Input Suspended",
            Self::Reconnecting { .. } => "Reconnecting",
            Self::Closed { .. } => "Closed",
        }
    }
}

/// Reason for input suspension while viewing continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendReason {
    /// Presentation evidence became stale or capture stalled.
    StaleView,
    /// Client window is hidden, minimized, or occluded.
    WindowHidden,
    /// Window lost user focus.
    FocusLost,
    /// Host notified temporary suspension.
    HostSuspension,
}

/// Reason for entering the reconnecting state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectReason {
    /// Transport connection dropped unexpectedly.
    TransportDrop,
    /// Handshake timed out.
    HandshakeTimeout,
    /// Heartbeat or authority renewal timeout.
    AuthorityTimeout,
    /// Protocol error on channel.
    ProtocolError,
}

/// Reason for permanent session closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// User or client cleanly closed the session.
    UserRequested,
    /// Host rejected observation or admission.
    HostRefused,
    /// Max reconnect attempts exceeded.
    MaxReconnectAttemptsExceeded,
    /// Fatal protocol or cryptographic error.
    FatalError,
}

/// Typed error outcomes for session operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// Operation invalid in the current session state.
    InvalidState,
    /// Control cannot be requested because view freshness has not been established.
    ViewNotFresh,
    /// Input is suspended due to stale presentation or window occlusion.
    InputSuspended,
    /// Coordinate mapping generation is stale; host crop/geometry changed.
    StaleMapping,
    /// Input lease has expired or was revoked.
    LeaseRevoked,
    /// Reconnect backoff has not yet expired.
    BackoffNotExpired,
    /// Arithmetic overflow in timestamp or sequence calculation.
    ArithmeticOverflow,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SessionError {}

/// Configuration for auto-reconnect backoff.
#[derive(Debug, Clone, Copy)]
pub struct ReconnectPolicy {
    /// Initial backoff delay in microseconds (default: 100 milliseconds).
    pub initial_backoff_us: u64,
    /// Maximum backoff ceiling in microseconds (default: 5 seconds).
    pub max_backoff_us: u64,
    /// Exponential multiplier factor (default: 2).
    pub backoff_factor: u64,
    /// Maximum number of reconnect attempts before closing (None = unlimited).
    pub max_attempts: Option<u32>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_backoff_us: 100_000,
            max_backoff_us: 5_000_000,
            backoff_factor: 2,
            max_attempts: Some(10),
        }
    }
}

impl ReconnectPolicy {
    /// Compute bounded exponential backoff for a given attempt (1-based).
    pub fn compute_backoff(&self, attempt: u32) -> u64 {
        if attempt <= 1 {
            return self.initial_backoff_us.min(self.max_backoff_us);
        }
        let shift = attempt.saturating_sub(1).min(10);
        let multiplier = self.backoff_factor.saturating_pow(shift);
        self.initial_backoff_us
            .saturating_mul(multiplier)
            .min(self.max_backoff_us)
    }

    /// Check if another reconnect attempt is permitted.
    pub fn can_retry(&self, attempt: u32) -> bool {
        match self.max_attempts {
            Some(max) => attempt <= max,
            None => true,
        }
    }
}

/// Decoder scheduling queue entry.
#[derive(Debug, Clone)]
pub struct QueuedPicture {
    pub frame_number: u64,
    pub is_reference: bool,
    pub byte_size: usize,
    pub captured_at_us: u64,
}

/// Decoder scheduling queue enforcing reference dependency preservation
/// and bounded queues for counts and bytes.
#[derive(Debug)]
pub struct DecoderScheduler {
    pub max_pictures: usize,
    pub max_bytes: usize,
    pictures: Vec<QueuedPicture>,
    current_bytes: usize,
    frames_decoded: u64,
    frames_dropped_decode: u64,
    frames_presented: u64,
    frames_skipped_presentation: u64,
}

impl DecoderScheduler {
    pub fn new(max_pictures: usize, max_bytes: usize) -> Self {
        Self {
            max_pictures: max_pictures.max(1),
            max_bytes: max_bytes.max(1024),
            pictures: Vec::with_capacity(max_pictures),
            current_bytes: 0,
            frames_decoded: 0,
            frames_dropped_decode: 0,
            frames_presented: 0,
            frames_skipped_presentation: 0,
        }
    }

    /// Enqueue a newly reassembled picture.
    /// Non-reference frames can be dropped if the queue is saturated,
    /// but reference pictures are preserved to avoid corrupting future frames.
    pub fn enqueue(&mut self, picture: QueuedPicture) -> Result<(), SessionError> {
        // Drop non-reference frames first if over limit
        while (self.pictures.len() >= self.max_pictures
            || self.current_bytes.saturating_add(picture.byte_size) > self.max_bytes)
            && !self.pictures.is_empty()
        {
            // Find oldest non-reference frame to discard
            if let Some(idx) = self.pictures.iter().position(|p| !p.is_reference) {
                let discarded = self.pictures.remove(idx);
                self.current_bytes = self.current_bytes.saturating_sub(discarded.byte_size);
                self.frames_dropped_decode = self.frames_dropped_decode.saturating_add(1);
            } else {
                // If all queued pictures are reference frames, we cannot drop them safely.
                // Discard oldest if strictly over count to prevent unbounded memory.
                if self.pictures.len() >= self.max_pictures {
                    let discarded = self.pictures.remove(0);
                    self.current_bytes = self.current_bytes.saturating_sub(discarded.byte_size);
                    self.frames_dropped_decode = self.frames_dropped_decode.saturating_add(1);
                }
                break;
            }
        }

        self.current_bytes = self.current_bytes.saturating_add(picture.byte_size);
        self.pictures.push(picture);
        Ok(())
    }

    /// Mark a frame as successfully decoded.
    pub fn on_frame_decoded(&mut self) {
        self.frames_decoded = self.frames_decoded.saturating_add(1);
    }

    /// Select the newest ready frame for presentation, discarding obsolete
    /// presentation work per Section 11.1 of the plan.
    /// Returns the frame number of the winning frame to present, if any.
    pub fn select_presentation_frame(&mut self, is_visible: bool) -> Option<u64> {
        if !is_visible || self.pictures.is_empty() {
            return None;
        }

        // Newest ready frame wins
        let winning = self.pictures.pop()?;
        self.current_bytes = self.current_bytes.saturating_sub(winning.byte_size);

        // Obsolete presentation work discarded: any older frames remaining in queue
        // that are non-reference can have their presentation skipped.
        let obsolete_count = u64::try_from(self.pictures.len()).unwrap_or(0);
        self.frames_skipped_presentation = self
            .frames_skipped_presentation
            .saturating_add(obsolete_count);

        self.frames_presented = self.frames_presented.saturating_add(1);
        Some(winning.frame_number)
    }

    pub fn queued_count(&self) -> usize {
        self.pictures.len()
    }

    pub fn queued_bytes(&self) -> usize {
        self.current_bytes
    }
}

/// Diagnostics snapshot reporting real-time metrics, queue depths, and reconnect state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDiagnostics {
    pub state_label: &'static str,
    pub reconnect_attempts: u32,
    pub total_reconnects: u32,
    pub last_backoff_us: u64,
    pub view_fresh: bool,
    pub is_controlling: bool,
    pub is_suspended: bool,
    pub current_lease: Option<u128>,
    pub frames_decoded: u64,
    pub frames_presented: u64,
    pub frames_dropped_decode: u64,
    pub frames_skipped_presentation: u64,
    pub stale_view_suspensions: u64,
    pub mapping_rejections: u64,
}

/// Shared client session core orchestrator.
pub struct ClientSession {
    state: SessionState,
    reconnect_policy: ReconnectPolicy,
    reconnect_attempt: u32,
    total_reconnects: u32,
    last_backoff_us: u64,

    // Authority & generations
    active_session_id: Option<RemoteSessionId>,
    active_binding: Option<ControlBinding>,
    active_lease: Option<InputLeaseId>,
    active_ticket: Option<InputTicketId>,
    expected_display_gen: DisplayGeometryGeneration,
    expected_viewport_gen: ViewportMappingGeneration,

    // Freshness & presentation tracking
    view_fresh: bool,
    window_visible: bool,
    scheduler: DecoderScheduler,

    // Metrics counters
    stale_view_suspensions: u64,
    mapping_rejections: u64,
    next_request_seq: u64,
}

impl ClientSession {
    /// Create a new client session in the Disconnected state.
    pub fn new(reconnect_policy: ReconnectPolicy) -> Self {
        Self {
            state: SessionState::Disconnected,
            reconnect_policy,
            reconnect_attempt: 0,
            total_reconnects: 0,
            last_backoff_us: 0,
            active_session_id: None,
            active_binding: None,
            active_lease: None,
            active_ticket: None,
            expected_display_gen: DisplayGeometryGeneration::INITIAL,
            expected_viewport_gen: ViewportMappingGeneration::INITIAL,
            view_fresh: false,
            window_visible: true,
            scheduler: DecoderScheduler::new(32, 2 * 1024 * 1024),
            stale_view_suspensions: 0,
            mapping_rejections: 0,
            next_request_seq: 1,
        }
    }

    /// Read current session state.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Initiate connection to host.
    pub fn connect(&mut self, now: ClientInstant) -> Result<(), SessionError> {
        match &self.state {
            SessionState::Disconnected | SessionState::Closed { .. } => {
                self.reconnect_attempt = 0;
                self.state = SessionState::Connecting {
                    attempt: 1,
                    started_at: now,
                };
                eprintln!("[StateTrace: Disconnected -> Connecting attempt=1]");
                Ok(())
            }
            _ => Err(SessionError::InvalidState),
        }
    }

    /// Called when host signals local approval is required.
    pub fn on_waiting_approval(
        &mut self,
        request: RemoteSessionId,
        host_deadline_us: u64,
    ) -> Result<(), SessionError> {
        match self.state {
            SessionState::Connecting { .. } => {
                self.state = SessionState::WaitingApproval {
                    request,
                    host_deadline_us,
                };
                eprintln!(
                    "[StateTrace: Connecting -> WaitingApproval request={request:?} host_deadline_us={host_deadline_us}]"
                );
                Ok(())
            }
            _ => Err(SessionError::InvalidState),
        }
    }

    /// Called when connection handshake and observation binding succeed.
    /// Transitions into `Viewing`. Note: Media arrival does NOT grant control.
    pub fn on_session_opened(
        &mut self,
        binding: ControlBinding,
        _now: ClientInstant,
    ) -> Result<(), SessionError> {
        match &self.state {
            SessionState::Connecting { .. } | SessionState::WaitingApproval { .. } => {
                let session_id = binding.remote_session;
                self.active_session_id = Some(session_id);
                self.active_binding = Some(binding);
                self.reconnect_attempt = 0;
                self.view_fresh = false; // Freshness must be proven by first presented frame

                self.state = SessionState::Viewing {
                    session: session_id,
                    binding,
                    view_fresh: false,
                };
                let bid = binding.id;
                eprintln!("[StateTrace: -> Viewing session={session_id:?} binding_id={bid}]");
                Ok(())
            }
            _ => Err(SessionError::InvalidState),
        }
    }

    /// Update view freshness evidence.
    /// If fresh evidence arrives, `view_fresh` is set.
    /// If view becomes stale while controlling, input is suspended immediately!
    pub fn update_view_freshness(
        &mut self,
        fresh: bool,
        _now: ClientInstant,
    ) -> Result<(), SessionError> {
        self.view_fresh = fresh;

        match &self.state {
            SessionState::Controlling { session, .. } if !fresh => {
                let session_id = *session;
                self.stale_view_suspensions = self.stale_view_suspensions.saturating_add(1);
                self.state = SessionState::Suspended {
                    session: session_id,
                    reason: SuspendReason::StaleView,
                };
                eprintln!(
                    "[StateTrace: Controlling -> Suspended reason=StaleView session={session_id:?}]"
                );
            }
            SessionState::Suspended {
                session,
                reason: SuspendReason::StaleView,
            } if fresh => {
                let session_id = *session;
                // If we still have an active lease, we can resume control
                if let (Some(lease), Some(ticket)) = (self.active_lease, self.active_ticket) {
                    self.state = SessionState::Controlling {
                        session: session_id,
                        lease,
                        ticket,
                        input_ready: true,
                    };
                    eprintln!(
                        "[StateTrace: Suspended -> Controlling (view freshness restored) lease={lease:?}]"
                    );
                } else {
                    // Lease expired/cleared during suspension: return to Viewing
                    self.state = SessionState::Viewing {
                        session: session_id,
                        binding: self.active_binding.unwrap_or_else(|| ControlBinding {
                            id: 0,
                            host_boot: HostBootId::from_raw(0),
                            os_session: fr_core::ids::OsSessionId::from_raw(0),
                            remote_session: session_id,
                        }),
                        view_fresh: true,
                    };
                    eprintln!("[StateTrace: Suspended -> Viewing (lease expired while stale)]");
                }
            }
            SessionState::Viewing {
                session, binding, ..
            } => {
                let s = *session;
                let b = *binding;
                self.state = SessionState::Viewing {
                    session: s,
                    binding: b,
                    view_fresh: fresh,
                };
            }
            _ => {}
        }
        Ok(())
    }

    /// Check if control can be requested.
    /// CONTROL CAN ONLY BE REQUESTED AFTER VIEW FRESHNESS IS ESTABLISHED.
    pub fn can_request_control(&self) -> bool {
        matches!(
            self.state,
            SessionState::Viewing {
                view_fresh: true,
                ..
            }
        )
    }

    /// Explicitly request input control.
    /// Requires view freshness to be established first.
    pub fn request_control(&mut self, now: ClientInstant) -> Result<u64, SessionError> {
        if !self.can_request_control() {
            return Err(SessionError::ViewNotFresh);
        }

        let session = self.active_session_id.ok_or(SessionError::InvalidState)?;
        let seq = self.next_request_seq;
        self.next_request_seq = self.next_request_seq.saturating_add(1);

        self.state = SessionState::RequestingControl {
            session,
            request_sequence: seq,
            requested_at: now,
        };
        eprintln!("[StateTrace: Viewing -> RequestingControl session={session:?} seq={seq}]");
        Ok(seq)
    }

    /// Called when host grants control.
    /// Establishes the new active lease.
    pub fn on_control_granted(
        &mut self,
        lease: InputLeaseId,
        ticket: InputTicketId,
    ) -> Result<(), SessionError> {
        match self.state {
            SessionState::RequestingControl { session, .. } => {
                self.active_lease = Some(lease);
                self.active_ticket = Some(ticket);

                self.state = SessionState::Controlling {
                    session,
                    lease,
                    ticket,
                    input_ready: true,
                };
                eprintln!(
                    "[StateTrace: RequestingControl -> Controlling session={session:?} lease={lease:?} ticket={ticket:?}]"
                );
                Ok(())
            }
            _ => Err(SessionError::InvalidState),
        }
    }

    /// Validate coordinate mapping generation before submitting coordinate input.
    /// Stale generations are rejected immediately per Section 7.2 of the plan.
    pub fn validate_coordinate_mapping(
        &mut self,
        display: DisplayGeometryGeneration,
        viewport: ViewportMappingGeneration,
    ) -> Result<(), SessionError> {
        if display != self.expected_display_gen || viewport != self.expected_viewport_gen {
            self.mapping_rejections = self.mapping_rejections.saturating_add(1);
            let edisp = self.expected_display_gen;
            let eview = self.expected_viewport_gen;
            eprintln!(
                "[StateTrace: CoordinateRejected stale_display={display:?} expected_display={edisp:?} stale_viewport={viewport:?} expected_viewport={eview:?}]"
            );
            return Err(SessionError::StaleMapping);
        }

        match &self.state {
            SessionState::Controlling {
                input_ready: true, ..
            } => Ok(()),
            SessionState::Suspended { .. } => Err(SessionError::InputSuspended),
            _ => Err(SessionError::InvalidState),
        }
    }

    /// Acknowledge an updated display geometry or host crop viewport mapping.
    pub fn update_mapping(
        &mut self,
        display: DisplayGeometryGeneration,
        viewport: ViewportMappingGeneration,
    ) {
        self.expected_display_gen = display;
        self.expected_viewport_gen = viewport;
        eprintln!("[StateTrace: MappingUpdated display={display:?} viewport={viewport:?}]");
    }

    /// Set window visibility (e.g. minimized/occluded).
    /// Occluded rendering does not count as presentation and suspends input.
    pub fn set_window_visible(&mut self, visible: bool) {
        self.window_visible = visible;
        if !visible && let SessionState::Controlling { session, .. } = self.state {
            self.state = SessionState::Suspended {
                session,
                reason: SuspendReason::WindowHidden,
            };
            eprintln!("[StateTrace: Controlling -> Suspended reason=WindowHidden]");
        }
    }

    /// Handle disconnect or transport failure.
    /// Implements TEARDOWN ORDER (plan §7.3):
    /// 1. Revoke input authority: any active lease is TERMINATED.
    /// 2. Clear tickets and held state.
    /// 3. Enter user-visible `Reconnecting` state with bounded backoff.
    ///
    /// CRITICAL: On resume/reconnect, media reconnect may preserve viewing,
    /// but NEVER silently restores a revoked input lease!
    pub fn on_disconnect(&mut self, reason: ReconnectReason, now: ClientInstant) {
        // Teardown: revoke input authority immediately
        let had_lease = self.active_lease.take();
        self.active_ticket = None;
        self.view_fresh = false;

        if let Some(lease) = had_lease {
            eprintln!(
                "[StateTrace: LeaseRevoked on disconnect - no silent resurrection permitted lease={lease:?}]"
            );
        }

        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        self.total_reconnects = self.total_reconnects.saturating_add(1);

        if !self.reconnect_policy.can_retry(self.reconnect_attempt) {
            self.state = SessionState::Closed {
                reason: CloseReason::MaxReconnectAttemptsExceeded,
            };
            let att = self.reconnect_attempt;
            eprintln!("[StateTrace: ReconnectFailed max_attempts_exceeded attempts={att}]");
            return;
        }

        let backoff = self
            .reconnect_policy
            .compute_backoff(self.reconnect_attempt);
        self.last_backoff_us = backoff;
        let next_retry = ClientInstant(now.0.saturating_add(backoff));

        self.state = SessionState::Reconnecting {
            attempt: self.reconnect_attempt,
            backoff_us: backoff,
            next_retry,
            reason,
        };

        let att = self.reconnect_attempt;
        let nretry = next_retry.0;
        eprintln!(
            "[StateTrace: Reconnecting attempt={att} backoff_us={backoff} next_retry_us={nretry} reason={reason:?}]"
        );
    }

    /// Tick the reconnect timer. If backoff has expired, transition to `Connecting`.
    pub fn tick_reconnect(&mut self, now: ClientInstant) -> Result<bool, SessionError> {
        match &self.state {
            SessionState::Reconnecting {
                attempt,
                next_retry,
                ..
            } => {
                if now.0 >= next_retry.0 {
                    let att = *attempt;
                    self.state = SessionState::Connecting {
                        attempt: att,
                        started_at: now,
                    };
                    eprintln!(
                        "[StateTrace: Reconnecting -> Connecting attempt={att} (timer fired)]"
                    );
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }

    /// Cleanly close the session.
    pub fn close(&mut self, reason: CloseReason) {
        self.active_lease = None;
        self.active_ticket = None;
        self.active_session_id = None;
        self.active_binding = None;
        self.view_fresh = false;

        self.state = SessionState::Closed { reason };
        eprintln!("[StateTrace: SessionClosed reason={reason:?}]");
    }

    /// Access the decoder scheduler.
    pub fn scheduler_mut(&mut self) -> &mut DecoderScheduler {
        &mut self.scheduler
    }

    /// Access the decoder scheduler (read-only).
    pub fn scheduler(&self) -> &DecoderScheduler {
        &self.scheduler
    }

    /// Generate real-time diagnostics snapshot.
    pub fn diagnostics(&self) -> SessionDiagnostics {
        let (is_controlling, is_suspended) = match &self.state {
            SessionState::Controlling { .. } => (true, false),
            SessionState::Suspended { .. } => (false, true),
            _ => (false, false),
        };

        SessionDiagnostics {
            state_label: self.state.display_label(),
            reconnect_attempts: self.reconnect_attempt,
            total_reconnects: self.total_reconnects,
            last_backoff_us: self.last_backoff_us,
            view_fresh: self.view_fresh,
            is_controlling,
            is_suspended,
            current_lease: self.active_lease.map(InputLeaseId::as_raw),
            frames_decoded: self.scheduler.frames_decoded,
            frames_presented: self.scheduler.frames_presented,
            frames_dropped_decode: self.scheduler.frames_dropped_decode,
            frames_skipped_presentation: self.scheduler.frames_skipped_presentation,
            stale_view_suspensions: self.stale_view_suspensions,
            mapping_rejections: self.mapping_rejections,
        }
    }
}
