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
    DisplayGeometryGeneration, InputLeaseId, InputTicketId, RemoteSessionId,
    ViewportMappingGeneration,
};
use fr_wire::{display::Catalog, negotiation::ControlBinding};
use std::{fmt, sync::Arc};

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
    /// Queued or decoding reference pictures retain the available capacity.
    DecoderBackpressure,
    /// A single picture exceeds this scheduler's byte allowance.
    PictureTooLarge,
    /// The completion does not belong to this scheduler's original decode.
    StaleDecode,
    /// Decoder admission is fenced until original cleanup has been confirmed.
    DecoderClosed,
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

/// One picture's metadata and retained-byte charge, before native decoding.
/// The native decoder owns its reference surfaces independently; presenting
/// or retiring this compressed-picture reservation never releases a surface.
#[derive(Debug, Clone)]
pub struct QueuedPicture {
    pub frame_number: u64,
    pub is_reference: bool,
    pub byte_size: usize,
    pub captured_at_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PictureStage {
    Queued,
    Decoding,
    Decoded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderPhase {
    Active,
    Closing { request_taken: bool },
    Closed,
}

#[derive(Debug)]
struct ScheduledPicture {
    picture: QueuedPicture,
    serial: u64,
    stage: PictureStage,
}

/// A non-cloneable completion identity for one admitted native decode. Keep it
/// with that decoder operation, never recreate it from a wire frame number.
/// Reconfiguration creates a new scheduler; an old token cannot complete a
/// same-number picture belonging to its replacement.
#[derive(Debug)]
pub struct DecodeWork {
    owner: Arc<()>,
    serial: u64,
    picture: QueuedPicture,
}

/// Original-owner identity for the native decoder's positive close callback.
/// The caller must confirm closure and release of the original reservations
/// before returning it. Issuing this token alone releases no count or bytes.
#[derive(Debug)]
pub struct DecoderClose {
    owner: Arc<()>,
}

impl DecodeWork {
    pub fn picture(&self) -> &QueuedPicture {
        &self.picture
    }
}

/// Bounded compressed-picture and presentation reservations. Decoding is FIFO
/// and explicit; admission alone is never evidence that a picture is ready.
/// Undecoded references and the one active decode cannot be displaced. After
/// decoding, only presentation work is retired here: codec-owned reference
/// surfaces remain the native decoder's responsibility.
#[derive(Debug)]
pub struct DecoderScheduler {
    max_pictures: usize,
    max_bytes: usize,
    owner: Arc<()>,
    phase: DecoderPhase,
    next_serial: Option<u64>,
    pictures: Vec<ScheduledPicture>,
    current_bytes: usize,
    frames_decoded: u64,
    frames_dropped_decode: u64,
    frames_presented: u64,
    frames_skipped_presentation: u64,
}

impl DecoderScheduler {
    pub fn new(max_pictures: usize, max_bytes: usize) -> Self {
        Self {
            max_pictures,
            max_bytes,
            owner: Arc::new(()),
            phase: DecoderPhase::Active,
            next_serial: Some(1),
            pictures: Vec::new(),
            current_bytes: 0,
            frames_decoded: 0,
            frames_dropped_decode: 0,
            frames_presented: 0,
            frames_skipped_presentation: 0,
        }
    }

    /// Enqueue a newly reassembled picture. Refusal changes no reservation:
    /// the caller retains the incoming picture and must backpressure or enter
    /// explicit reference recovery, never continue after silently losing it.
    /// Only completed presentation candidates and not-yet-decoding nonreference
    /// pictures can make room. Counts and bytes are checked before mutation.
    pub fn enqueue(&mut self, picture: QueuedPicture) -> Result<(), SessionError> {
        if self.phase != DecoderPhase::Active {
            return Err(SessionError::DecoderClosed);
        }
        let retained_limit = self
            .max_bytes
            .checked_sub(picture.byte_size)
            .ok_or(SessionError::PictureTooLarge)?;
        let serial = self.next_serial.ok_or(SessionError::ArithmeticOverflow)?;
        let (mut count, mut bytes, mut discard) = (self.pictures.len(), self.current_bytes, 0usize);
        for queued in &self.pictures {
            if count < self.max_pictures && bytes <= retained_limit {
                break;
            }
            if Self::replaceable(queued) {
                count -= 1;
                bytes -= queued.picture.byte_size;
                discard += 1;
            }
        }
        if count >= self.max_pictures || bytes > retained_limit {
            return Err(SessionError::DecoderBackpressure);
        }
        let needed = count + 1;
        if needed > self.pictures.capacity() {
            self.pictures
                .try_reserve_exact(needed - self.pictures.len())
                .map_err(|_| SessionError::DecoderBackpressure)?;
        }

        self.pictures.retain(|queued| {
            if discard == 0 || !Self::replaceable(queued) {
                return true;
            }
            discard -= 1;
            if queued.stage == PictureStage::Decoded {
                self.frames_skipped_presentation =
                    self.frames_skipped_presentation.saturating_add(1);
            } else {
                self.frames_dropped_decode = self.frames_dropped_decode.saturating_add(1);
            }
            false
        });
        // The preflight proves this addition fits the configured usize bound.
        self.current_bytes = bytes + picture.byte_size;
        self.pictures.push(ScheduledPicture {
            picture,
            serial,
            stage: PictureStage::Queued,
        });
        self.next_serial = serial.checked_add(1);
        Ok(())
    }

    fn replaceable(queued: &ScheduledPicture) -> bool {
        queued.stage == PictureStage::Decoded
            || (queued.stage == PictureStage::Queued && !queued.picture.is_reference)
    }

    /// Begin the next FIFO decode. At most one operation is outstanding, and
    /// its count/bytes remain charged until completion and presentation retire
    /// it. Abandonment requires retiring the decoder and this original owner.
    pub fn next_decode(&mut self) -> Option<DecodeWork> {
        if self.phase != DecoderPhase::Active
            || self
                .pictures
                .iter()
                .any(|queued| queued.stage == PictureStage::Decoding)
        {
            return None;
        }
        let queued = self
            .pictures
            .iter_mut()
            .find(|queued| queued.stage == PictureStage::Queued)?;
        queued.stage = PictureStage::Decoding;
        Some(DecodeWork {
            owner: Arc::clone(&self.owner),
            serial: queued.serial,
            picture: queued.picture.clone(),
        })
    }

    /// Publish a successful native completion under its original local token.
    /// A foreign or retired decoder's callback cannot mark a current picture
    /// ready, even when both wire frame numbers are identical.
    #[allow(clippy::needless_pass_by_value)] // Consume this one completion identity.
    pub fn on_frame_decoded(&mut self, work: DecodeWork) -> Result<(), SessionError> {
        if self.phase == DecoderPhase::Closed || !Arc::ptr_eq(&self.owner, &work.owner) {
            return Err(SessionError::StaleDecode);
        }
        let queued = self
            .pictures
            .iter_mut()
            .find(|queued| queued.serial == work.serial && queued.stage == PictureStage::Decoding)
            .ok_or(SessionError::StaleDecode)?;
        queued.stage = PictureStage::Decoded;
        self.frames_decoded = self.frames_decoded.saturating_add(1);
        Ok(())
    }

    /// Select the newest ready frame for presentation, discarding obsolete
    /// presentation work per Section 11.1 of the plan.
    /// Returns the frame number of the winning frame to present, if any.
    pub fn select_presentation_frame(&mut self, is_visible: bool) -> Option<u64> {
        if !is_visible || self.phase != DecoderPhase::Active {
            return None;
        }

        let winning = self
            .pictures
            .iter()
            .rev()
            .find(|queued| queued.stage == PictureStage::Decoded)?
            .serial;
        let mut frame = None;
        self.pictures.retain(|queued| {
            if queued.stage != PictureStage::Decoded {
                return true;
            }
            self.current_bytes -= queued.picture.byte_size;
            if queued.serial == winning {
                frame = Some(queued.picture.frame_number);
            } else {
                self.frames_skipped_presentation =
                    self.frames_skipped_presentation.saturating_add(1);
            }
            false
        });
        self.frames_presented = self.frames_presented.saturating_add(1);
        frame
    }

    pub fn queued_count(&self) -> usize {
        self.pictures.len()
    }

    pub fn queued_bytes(&self) -> usize {
        self.current_bytes
    }

    /// Stop admission, decode starts and presentation immediately. Retain all
    /// original reservations, including in-flight work, until positive native
    /// closure. Repeated fences never replace an outstanding close identity.
    pub fn fence(&mut self) {
        if self.phase == DecoderPhase::Active {
            self.phase = DecoderPhase::Closing {
                request_taken: false,
            };
        }
    }

    /// Take the one close callback identity for this original decoder. Call
    /// `on_decoder_closed` only after its native owner has actually closed.
    pub fn take_close_request(&mut self) -> Option<DecoderClose> {
        let DecoderPhase::Closing { request_taken } = &mut self.phase else {
            return None;
        };
        if *request_taken {
            return None;
        }
        *request_taken = true;
        Some(DecoderClose {
            owner: Arc::clone(&self.owner),
        })
    }

    /// Positive closure retires only the original decoder's reservations. A
    /// late decode callback is now obsolete; reopening is a separate step.
    #[allow(clippy::needless_pass_by_value)] // Consume the original close callback identity.
    pub fn on_decoder_closed(&mut self, closed: DecoderClose) -> Result<(), SessionError> {
        if self.phase
            != (DecoderPhase::Closing {
                request_taken: true,
            })
            || !Arc::ptr_eq(&self.owner, &closed.owner)
        {
            return Err(SessionError::StaleDecode);
        }
        self.phase = DecoderPhase::Closed;
        self.pictures.clear();
        self.current_bytes = 0;
        Ok(())
    }

    /// Start a replacement only after original native cleanup is confirmed.
    /// This owner identity is fresh even when wire frame numbers restart.
    pub fn restart_after_close(&mut self) -> Result<(), SessionError> {
        if self.phase != DecoderPhase::Closed {
            return Err(SessionError::InvalidState);
        }
        self.owner = Arc::new(());
        self.next_serial = Some(1);
        self.phase = DecoderPhase::Active;
        Ok(())
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
    window_focused: bool,
    scheduler: DecoderScheduler,

    // Multi-display topology & targeting
    current_catalog: Option<Catalog>,
    target_display: Option<u128>,
    selected_displays: Vec<u128>,

    // Metrics counters
    stale_view_suspensions: u64,
    mapping_rejections: u64,
    next_request_seq: Option<u64>,
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
            window_focused: true,
            scheduler: DecoderScheduler::new(32, 2 * 1024 * 1024),
            current_catalog: None,
            target_display: None,
            selected_displays: Vec::new(),
            stale_view_suspensions: 0,
            mapping_rejections: 0,
            next_request_seq: Some(1),
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
        if binding.id == 0
            || binding.host_boot.as_raw() == 0
            || binding.os_session.as_raw() == 0
            || binding.remote_session.as_raw() == 0
        {
            return Err(SessionError::InvalidState);
        }
        match &self.state {
            SessionState::Connecting { .. } | SessionState::WaitingApproval { .. } => {
                let session_id = binding.remote_session;
                self.active_session_id = Some(session_id);
                self.active_binding = Some(binding);
                self.active_lease = None;
                self.active_ticket = None;
                self.current_catalog = None;
                self.target_display = None;
                self.selected_displays.clear();
                self.expected_display_gen = DisplayGeometryGeneration::INITIAL;
                self.expected_viewport_gen = ViewportMappingGeneration::INITIAL;
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

    /// Project current evidence from the original renderer/freshness owner.
    /// Losing that evidence retires the pending request and local grant. Later
    /// freshness permits a new explicit request; it cannot resurrect old IDs.
    /// The actual input owner independently enforces ticket and lease expiry.
    pub fn update_view_freshness(
        &mut self,
        fresh: bool,
        _now: ClientInstant,
    ) -> Result<(), SessionError> {
        if !self.is_observing() {
            return Err(SessionError::InvalidState);
        }
        if fresh && (!self.window_visible || !self.window_focused) {
            return Err(SessionError::InputSuspended);
        }
        self.view_fresh = fresh;
        if !fresh
            && matches!(
                self.state,
                SessionState::Controlling { .. } | SessionState::RequestingControl { .. }
            )
        {
            self.stale_view_suspensions = self.stale_view_suspensions.saturating_add(1);
            self.suspend_control(SuspendReason::StaleView);
        }
        if matches!(self.state, SessionState::Viewing { .. })
            || (fresh && matches!(self.state, SessionState::Suspended { .. }))
        {
            self.show_view();
        }
        Ok(())
    }

    fn is_observing(&self) -> bool {
        matches!(
            self.state,
            SessionState::Viewing { .. }
                | SessionState::RequestingControl { .. }
                | SessionState::Controlling { .. }
                | SessionState::Suspended { .. }
        )
    }

    fn show_view(&mut self) {
        if let Some(binding) = self.active_binding {
            self.state = SessionState::Viewing {
                session: binding.remote_session,
                binding,
                view_fresh: self.view_fresh,
            };
        }
    }

    fn suspend_control(&mut self, reason: SuspendReason) {
        self.active_lease = None;
        self.active_ticket = None;
        self.view_fresh = false;
        if self.is_observing()
            && let Some(session) = self.active_session_id
        {
            self.state = SessionState::Suspended { session, reason };
        }
    }

    /// Check if control can be requested.
    /// CONTROL CAN ONLY BE REQUESTED AFTER VIEW FRESHNESS IS ESTABLISHED.
    pub fn can_request_control(&self) -> bool {
        self.window_visible
            && self.window_focused
            && self.current_catalog.is_none_or(|catalog| {
                self.target_display
                    .is_some_and(|target| catalog.find(target).is_some())
            })
            && matches!(
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
        let seq = self
            .next_request_seq
            .ok_or(SessionError::ArithmeticOverflow)?;
        self.next_request_seq = seq.checked_add(1);

        self.state = SessionState::RequestingControl {
            session,
            request_sequence: seq,
            requested_at: now,
        };
        eprintln!("[StateTrace: Viewing -> RequestingControl session={session:?} seq={seq}]");
        Ok(seq)
    }

    /// Project a validated host grant for the exact outstanding request.
    /// IDs alone never satisfy a replacement request. The containing input
    /// owner must already have checked the authenticated grant's deadlines;
    /// this UI/session projection does not authorize native input by itself.
    pub fn on_control_granted(
        &mut self,
        request_sequence: u64,
        lease: InputLeaseId,
        ticket: InputTicketId,
    ) -> Result<(), SessionError> {
        match self.state {
            SessionState::RequestingControl {
                session,
                request_sequence: expected,
                ..
            } if request_sequence == expected
                && self.window_visible
                && self.window_focused
                && self.view_fresh =>
            {
                if lease.as_raw() == 0 || ticket.as_raw() == 0 {
                    self.invalidate_control();
                    return Err(SessionError::LeaseRevoked);
                }
                self.active_lease = Some(lease);
                self.active_ticket = Some(ticket);

                self.state = SessionState::Controlling {
                    session,
                    lease,
                    ticket,
                    input_ready: true,
                };
                eprintln!("[StateTrace: RequestingControl -> Controlling]");
                Ok(())
            }
            _ => Err(SessionError::InvalidState),
        }
    }

    /// Retire the local input grant and any pending control request while
    /// retaining observation. A later fresh frame permits a new explicit
    /// request; it cannot resurrect the previous lease or ticket.
    ///
    /// The connection owner must separately send the host's release/close
    /// record. This local fence does not claim host cleanup has completed.
    pub fn invalidate_control(&mut self) {
        self.active_lease = None;
        self.active_ticket = None;
        self.view_fresh = false;
        if self.is_observing() {
            self.show_view();
        }
    }

    /// Begin explicit decoder/configuration recovery: fence input first, then
    /// retain the original decode reservations through native close. Ordinary
    /// focus or view-readiness changes do not close observation's decoder.
    pub fn fence_decoder(&mut self) {
        self.invalidate_control();
        self.scheduler.fence();
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
        if display != self.expected_display_gen || viewport != self.expected_viewport_gen {
            self.invalidate_control();
        }
        self.expected_display_gen = display;
        self.expected_viewport_gen = viewport;
        eprintln!("[StateTrace: MappingUpdated display={display:?} viewport={viewport:?}]");
    }

    /// Install forward catalog evidence. Changed target geometry retires the
    /// pending request/grant; removing its display never silently targets a
    /// different display. A repeated identical revision is harmless, while a
    /// regressed or contradictory revision cannot replace the current catalog.
    pub fn on_catalog_received(&mut self, catalog: Catalog) -> Result<(), SessionError> {
        if let Some(previous) = self.current_catalog {
            if catalog.revision() < previous.revision()
                || (catalog.revision() == previous.revision() && catalog != previous)
            {
                return Err(SessionError::StaleMapping);
            }
            if catalog == previous {
                return Ok(());
            }
        }
        let target_changed = self.target_display.is_some_and(|target| {
            self.current_catalog.and_then(|old| old.find(target)) != catalog.find(target)
        });
        let geometry = self
            .target_display
            .and_then(|target| catalog.find(target))
            .map(|display| display.geometry)
            .or_else(|| catalog.geometry_generation());
        if target_changed || geometry.is_some_and(|value| value != self.expected_display_gen) {
            self.invalidate_control();
        }
        if let Some(geometry) = geometry {
            self.expected_display_gen = geometry;
        }
        self.selected_displays
            .retain(|handle| catalog.find(*handle).is_some());
        if self
            .target_display
            .is_some_and(|target| catalog.find(target).is_none())
        {
            self.target_display = None;
        }

        self.current_catalog = Some(catalog);
        Ok(())
    }

    /// Explicitly select a display to observe.
    pub fn select_display(&mut self, handle: u128) -> Result<(), SessionError> {
        let catalog = self
            .current_catalog
            .as_ref()
            .ok_or(SessionError::InvalidState)?;
        let display = catalog.find(handle).ok_or(SessionError::InvalidState)?;
        if !self.selected_displays.contains(&handle) {
            self.selected_displays.push(handle);
        }
        if self.target_display.is_none() {
            self.invalidate_control();
            self.target_display = Some(handle);
            self.expected_display_gen = display.geometry;
        }
        Ok(())
    }

    /// Unselect an observed display.
    pub fn unselect_display(&mut self, handle: u128) {
        self.selected_displays.retain(|h| *h != handle);
        if self.target_display == Some(handle) {
            self.invalidate_control();
            self.target_display = None;
        }
    }

    /// Set the single controller's explicit target display.
    pub fn set_target_display(&mut self, handle: u128) -> Result<(), SessionError> {
        let catalog = self
            .current_catalog
            .as_ref()
            .ok_or(SessionError::InvalidState)?;
        let display = catalog.find(handle).ok_or(SessionError::InvalidState)?;
        if self.target_display != Some(handle) {
            self.invalidate_control();
        }
        self.target_display = Some(handle);
        self.expected_display_gen = display.geometry;
        if !self.selected_displays.contains(&handle) {
            self.selected_displays.push(handle);
        }
        Ok(())
    }

    /// Read currently explicit target display handle.
    pub fn target_display(&self) -> Option<u128> {
        self.target_display
    }

    /// Read currently cached display catalog.
    pub fn current_catalog(&self) -> Option<&Catalog> {
        self.current_catalog.as_ref()
    }

    /// Read currently selected display handles.
    pub fn selected_displays(&self) -> &[u128] {
        &self.selected_displays
    }

    /// Visibility loss fences active and pending control. Making the window
    /// visible again proves no presentation and restores no grant.
    pub fn set_window_visible(&mut self, visible: bool) {
        self.window_visible = visible;
        if !visible {
            self.suspend_control(SuspendReason::WindowHidden);
        } else if self.window_focused && matches!(self.state, SessionState::Suspended { .. }) {
            self.show_view();
        }
    }

    /// Handle client window focus loss.
    ///
    /// Retire both a pending control request and any active grant before local
    /// cleanup. Host release/closure remains the connection owner's obligation.
    pub fn on_focus_loss(&mut self) {
        self.window_focused = false;
        self.suspend_control(SuspendReason::FocusLost);
    }

    /// Handle client window focus restoration.
    ///
    /// Focus alone cannot prove a fresh view or restore input. Returns `false`:
    /// after new presentation evidence a separate explicit request is required.
    pub fn on_focus_gained(&mut self, _now: ClientInstant) -> Result<bool, SessionError> {
        self.window_focused = true;
        if self.window_visible && matches!(self.state, SessionState::Suspended { .. }) {
            self.show_view();
        }
        Ok(false)
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
        let had_lease = self.active_lease.take().is_some();
        self.active_ticket = None;
        self.view_fresh = false;
        self.scheduler.fence();

        if had_lease {
            eprintln!(
                "[StateTrace: LeaseRevoked on disconnect - no silent resurrection permitted]"
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
        self.scheduler.fence();

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

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::{HostBootId, OsSessionId};

    #[test]
    fn exhausted_control_requests_never_reuse_the_last_identity() {
        let mut session = ClientSession::new(ReconnectPolicy::default());
        let now = ClientInstant(1);
        session.connect(now).unwrap();
        session
            .on_session_opened(
                ControlBinding {
                    id: 1,
                    host_boot: HostBootId::from_raw(1),
                    os_session: OsSessionId::from_raw(1),
                    remote_session: RemoteSessionId::from_raw(1),
                },
                now,
            )
            .unwrap();
        session.update_view_freshness(true, now).unwrap();
        session.next_request_seq = Some(u64::MAX);
        assert_eq!(session.request_control(now), Ok(u64::MAX));
        session.invalidate_control();
        session.update_view_freshness(true, now).unwrap();
        assert_eq!(
            session.request_control(now),
            Err(SessionError::ArithmeticOverflow)
        );
        assert_eq!(
            session.on_control_granted(
                u64::MAX,
                InputLeaseId::from_raw(1),
                InputTicketId::from_raw(1)
            ),
            Err(SessionError::InvalidState)
        );
        assert_eq!(session.diagnostics().current_lease, None);
    }

    #[test]
    fn exhausted_decode_serial_refuses_without_discarding_queued_work() {
        let mut scheduler = DecoderScheduler::new(1, 1);
        scheduler.next_serial = Some(u64::MAX);
        let picture = QueuedPicture {
            frame_number: 1,
            is_reference: false,
            byte_size: 1,
            captured_at_us: 1,
        };
        scheduler.enqueue(picture.clone()).unwrap();
        assert_eq!(
            scheduler.enqueue(picture),
            Err(SessionError::ArithmeticOverflow)
        );
        assert_eq!(scheduler.queued_count(), 1);
        assert_eq!(scheduler.queued_bytes(), 1);
        let work = scheduler.next_decode().unwrap();
        scheduler.on_frame_decoded(work).unwrap();
        assert_eq!(scheduler.select_presentation_frame(true), Some(1));
    }
}
