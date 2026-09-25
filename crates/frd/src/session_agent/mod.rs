//! Interactive-session agent coordinating consent, indicator, revoke, input lease ownership, and watchdog (plan §§5.3, 7.3, 15.2).
//!
//! Owns:
//! 1. Local approval UI gating: any new observation is strictly gated before user prompt.
//! 2. Always-visible sharing indicator with immediate synchronous revoke.
//! 3. Remote-only held-state tracking and honest crash uncertainty reporting.
//! 4. Platform permission state surfacing and OS session lifecycle transitions.
//! 5. Active session idle sleep inhibitor ref-counted across sessions.
//! 6. Local input priority suspending remote lease where distinguishable.
//! 7. Submission-time checkpoint verification immediately before every OS submission.

pub mod approval;
pub mod held_state;
mod lifetime;
pub use lifetime::AgentIdentity;
pub mod indicator;
pub mod local_priority;
pub mod macos_injection;
pub mod permissions;
pub mod sleep_inhibitor;
#[cfg(target_os = "linux")]
pub mod source;

pub use approval::{
    ApprovalManager, ApprovalMode, ApprovalRecord, ApprovalState, AudioScope, DenialReason,
    GrantedScope, PeerIdentity, RequestedScope, SessionRole,
};
pub use held_state::{ReleaseCertainty, RemoteHeldTracker, UncertainReleaseReport};
pub use indicator::{
    ConnectedSession, ImmediateRevokeOutcome, IndicatorDisplayState, OsCleanupTracker,
    SessionCapabilitiesInUse, SharingIndicator,
};
pub use local_priority::{
    Distinguishability, LocalInputPriority, LocalPriorityConfig, LocalPriorityOutcome,
    LocalPrioritySuspended,
};
pub use macos_injection::{
    MacOsEventPoster, MacOsInputSink, MacOsKeyCode, PostedCgEvent, RecordingPoster,
    hid_to_macos_keycode,
};
pub use permissions::{
    PermissionKind, PermissionStatus, PermissionsManager, PlatformKind, PlatformPermissionError,
};
pub use sleep_inhibitor::{
    InhibitorAction, InhibitorError, InhibitorLogEntry, SleepInhibitor, SleepInhibitorPlatform,
    UnavailableInhibitorPlatform,
};

use crate::input_watchdog::{Control as InputControl, StopReason};
use fr_core::{
    ids::RemoteSessionId,
    input::InputBounds,
    input_submission::{Operation, RevokeHandle},
    time::HostInstant,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Typed refusal reason when an operation fails the submission-time checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionRefusal {
    /// Session was not approved or does not possess the Controller role.
    NotApproved,
    /// Authority has been revoked.
    Revoked,
    /// Required platform permission (e.g. macOS Accessibility) is missing or OS session is locked.
    Permission(PlatformPermissionError),
    /// Remote input is temporarily suspended due to local physical input activity.
    LocalPrioritySuspended(HostInstant),
    /// Input coordinate is outside admitted desktop bounds.
    OutOfBounds,
    /// Requested input action is unsupported on this platform.
    Unsupported,
    /// Control lease has expired at the submission checkpoint.
    LeaseExpired,
}

impl core::fmt::Display for SubmissionRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotApproved => write!(f, "Session not approved for input control"),
            Self::Revoked => write!(f, "Input authority has been revoked"),
            Self::Permission(e) => write!(f, "Platform permission error: {e}"),
            Self::LocalPrioritySuspended(until) => {
                write!(
                    f,
                    "Remote input suspended by local activity until {until:?}"
                )
            }
            Self::OutOfBounds => write!(f, "Input coordinates out of desktop bounds"),
            Self::Unsupported => write!(f, "Input operation unsupported"),
            Self::LeaseExpired => write!(f, "Input lease expired at submission checkpoint"),
        }
    }
}

impl std::error::Error for SubmissionRefusal {}

/// The interactive session agent running in the user's desktop context.
pub struct SessionAgent {
    approval: ApprovalManager,
    indicator: SharingIndicator,
    held_state: RemoteHeldTracker,
    permissions: PermissionsManager,
    sleep_inhibitor: SleepInhibitor,
    local_priority: LocalInputPriority,
    bounds: InputBounds,
    revoked: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    sources: Arc<std::sync::Mutex<source::Sources>>,
    /// Opt-in remote-control profile; None keeps the agent observation-only.
    #[cfg(target_os = "linux")]
    control: Option<source::desktop::ControlProfile>,
}

impl SessionAgent {
    pub fn new(
        approval_mode: ApprovalMode,
        platform: PlatformKind,
        os_session_id: u32,
        bounds: InputBounds,
    ) -> Self {
        let permissions = PermissionsManager::new(platform, os_session_id);
        let indicator = SharingIndicator::new();
        let revoked = Arc::new(AtomicBool::new(false));

        // Connect the revoked atomic flag to indicator
        let revoked_flag = revoked.clone();
        indicator.register_custom_revoker(move || {
            revoked_flag.store(true, Ordering::Release);
        });

        #[cfg(target_os = "linux")]
        let sources = Arc::new(std::sync::Mutex::new(source::Sources::default()));
        #[cfg(target_os = "linux")]
        {
            let weak = Arc::downgrade(&sources);
            indicator.register_custom_revoker(move || {
                if let Some(sources) = weak.upgrade() {
                    sources
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .close();
                }
            });
        }

        Self {
            #[cfg(target_os = "linux")]
            sources,
            #[cfg(target_os = "linux")]
            control: None,
            approval: ApprovalManager::new(approval_mode),
            indicator,
            held_state: RemoteHeldTracker::new(),
            permissions,
            sleep_inhibitor: SleepInhibitor::default(),
            local_priority: LocalInputPriority::default(),
            bounds,
            revoked,
        }
    }

    pub fn approval(&self) -> &ApprovalManager {
        &self.approval
    }

    pub fn approval_mut(&mut self) -> &mut ApprovalManager {
        &mut self.approval
    }

    pub fn indicator(&self) -> &SharingIndicator {
        &self.indicator
    }

    pub fn held_state(&self) -> &RemoteHeldTracker {
        &self.held_state
    }

    pub fn held_state_mut(&mut self) -> &mut RemoteHeldTracker {
        &mut self.held_state
    }

    pub fn permissions(&self) -> &PermissionsManager {
        &self.permissions
    }

    pub fn permissions_mut(&mut self) -> &mut PermissionsManager {
        &mut self.permissions
    }

    pub fn sleep_inhibitor(&self) -> &SleepInhibitor {
        &self.sleep_inhibitor
    }

    pub fn sleep_inhibitor_mut(&mut self) -> &mut SleepInhibitor {
        &mut self.sleep_inhibitor
    }

    pub fn local_priority(&self) -> &LocalInputPriority {
        &self.local_priority
    }

    pub fn local_priority_mut(&mut self) -> &mut LocalInputPriority {
        &mut self.local_priority
    }

    pub fn bounds(&self) -> InputBounds {
        self.bounds
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Register an external `InputControl` with the indicator for synchronous revocation.
    pub fn register_input_control(&self, control: InputControl) {
        self.indicator.register_input_control(control);
    }

    /// Register an external `RevokeHandle` with the indicator for synchronous revocation.
    pub fn register_revoke_handle(&self, handle: RevokeHandle) {
        self.indicator.register_revoke_handle(handle);
    }

    /// Request a new session. Observation is strictly gated until approved.
    pub fn request_session(
        &mut self,
        session_id: RemoteSessionId,
        peer: &PeerIdentity,
        requested: &RequestedScope,
        now: HostInstant,
    ) -> Result<ApprovalState, DenialReason> {
        let state =
            self.approval
                .request_approval(session_id, peer.clone(), requested.clone(), now)?;

        if let ApprovalState::Approved(ref grant) = state {
            // If immediately approved (e.g. Unattended or pre-authorized), update indicator and sleep inhibitor
            self.on_grant_activated(session_id, &peer.node_name, grant, now);
        }

        Ok(state)
    }

    /// Approve a pending session.
    pub fn approve_session(
        &mut self,
        session_id: RemoteSessionId,
        granted: GrantedScope,
        now: HostInstant,
    ) -> Result<GrantedScope, DenialReason> {
        let peer_name = self
            .approval
            .get_record(session_id)
            .map_or_else(|| "remote-peer".into(), |r| r.peer.node_name.clone());

        let grant = self.approval.approve(session_id, granted, now)?;
        self.on_grant_activated(session_id, &peer_name, &grant, now);
        Ok(grant)
    }

    fn on_grant_activated(
        &mut self,
        session_id: RemoteSessionId,
        peer_name: &str,
        grant: &GrantedScope,
        now: HostInstant,
    ) {
        let capabilities = SessionCapabilitiesInUse::from_granted_scope(grant);
        let conn = ConnectedSession {
            session_id,
            device_name: peer_name.to_string(),
            role: grant.role,
            capabilities,
            connected_at: now,
        };
        self.indicator.add_connected_session(conn);
        let _ = self.sleep_inhibitor.acquire(session_id, now);
    }

    /// Returns the list of all currently connected sessions with roles and capabilities.
    pub fn connected_sessions(&self) -> Vec<ConnectedSession> {
        self.indicator.connected_sessions()
    }

    /// Returns a specific connected session by ID.
    pub fn connected_session(&self, session_id: RemoteSessionId) -> Option<ConnectedSession> {
        self.indicator.connected_session(session_id)
    }

    /// Register an external `InputControl` scoped to a specific session.
    pub fn register_session_input_control(
        &self,
        session_id: RemoteSessionId,
        control: InputControl,
    ) {
        self.indicator
            .register_session_input_control(session_id, control);
    }

    /// Register an external `RevokeHandle` scoped to a specific session.
    pub fn register_session_revoke_handle(
        &self,
        session_id: RemoteSessionId,
        handle: RevokeHandle,
    ) {
        self.indicator
            .register_session_revoke_handle(session_id, handle);
    }

    /// Register a custom revocation closure scoped to a specific session.
    pub fn register_session_custom_revoker(
        &self,
        session_id: RemoteSessionId,
        revoker: impl Fn() + Send + Sync + 'static,
    ) {
        self.indicator
            .register_session_custom_revoker(session_id, revoker);
    }

    /// Synchronously revoke authority for ONE specific connected session.
    ///
    /// Other connected sessions remain unaffected.
    /// If the revoked session had the controller role, synthesizes cleanup releases.
    pub fn revoke_session(
        &mut self,
        session_id: RemoteSessionId,
        now: HostInstant,
        reason: StopReason,
    ) -> Option<(ImmediateRevokeOutcome, Vec<Operation>)> {
        let was_controller = self
            .connected_session(session_id)
            .is_some_and(|s| s.role == SessionRole::Controller);

        self.approval.revoke(session_id);
        let _ = self.sleep_inhibitor.release(session_id, now);

        let outcome = self.indicator.revoke_session(session_id, now, reason)?;

        let releases = if was_controller {
            self.held_state.synthesize_cleanup_releases()
        } else {
            Vec::new()
        };

        Some((outcome, releases))
    }

    /// Deny a pending session request.
    pub fn deny_session(
        &mut self,
        session_id: RemoteSessionId,
        reason: DenialReason,
    ) -> Result<(), DenialReason> {
        self.approval.deny(session_id, reason)
    }

    /// Change approval mode. Revokes all existing grants immediately to prevent stale authority.
    pub fn set_approval_mode(&mut self, mode: ApprovalMode, now: HostInstant) {
        if self.approval.mode() != mode {
            self.approval.set_mode(mode);
            self.immediate_revoke(now, StopReason::AuthorityEnded);
        }
    }

    /// Check if observation (pixels/thumbnails/audio/clipboard) is admitted for this session.
    pub fn is_observation_admitted(&self, session_id: RemoteSessionId) -> bool {
        !self.is_revoked() && self.approval.is_observation_admitted(session_id)
    }

    /// Check if control (keyboard/mouse injection) is admitted for this session.
    pub fn is_control_admitted(&self, session_id: RemoteSessionId) -> bool {
        !self.is_revoked() && self.approval.is_control_admitted(session_id)
    }

    /// Synchronously execute immediate revocation at the authority decision point.
    ///
    /// The synchronous latency is measured down to the nanosecond and logged.
    /// OS native cleanup is tracked separately via `OsCleanupTracker`.
    /// Releases sleep inhibitor and synthesizes cleanup release operations for held keys.
    pub fn immediate_revoke(
        &mut self,
        now: HostInstant,
        reason: StopReason,
    ) -> (ImmediateRevokeOutcome, Vec<Operation>) {
        self.revoked.store(true, Ordering::Release);
        let outcome = self.indicator.immediate_revoke(now, reason);

        // Emergency release sleep inhibitor
        let _ = self.sleep_inhibitor.emergency_release_all(now);

        // Synthesize cleanup releases for any remotely held keys/buttons
        let releases = self.held_state.synthesize_cleanup_releases();

        (outcome, releases)
    }

    /// Submission-time checkpoint: re-checked immediately before EVERY native OS injection.
    ///
    /// Invariants:
    /// 1. Session must be actively approved for control.
    /// 2. Authority must not be revoked.
    /// 3. Remote input must not be suspended due to local physical input activity.
    /// 4. Platform must possess required input permissions (Accessibility on macOS, portal on Wayland, session not locked).
    /// 5. Coordinates must fall within admitted bounds.
    ///
    /// If all checks pass, updates the remote-only held-state tracker.
    pub fn verify_and_track_submission(
        &mut self,
        session_id: RemoteSessionId,
        operation: &Operation,
        now: HostInstant,
    ) -> Result<(), SubmissionRefusal> {
        // Check 1: Revocation
        if self.is_revoked() {
            return Err(SubmissionRefusal::Revoked);
        }

        // Check 2: Control admission
        if !self.is_control_admitted(session_id) {
            return Err(SubmissionRefusal::NotApproved);
        }

        // Check 2b: Lease expiry check immediately before submission
        if let Some(grant) = self.approval.granted_scope(session_id)
            && let Some(expires_at) = grant.expires_at
            && now >= expires_at
        {
            return Err(SubmissionRefusal::LeaseExpired);
        }

        // Check 3: Local input priority suspension
        if let Err(susp) = self.local_priority.verify_submission_allowed(now) {
            return Err(SubmissionRefusal::LocalPrioritySuspended(
                susp.suspended_until,
            ));
        }

        // Check 4: Platform permissions & OS session state
        if let Err(perm_err) = self.permissions.verify_input_injection() {
            return Err(SubmissionRefusal::Permission(perm_err));
        }

        // Check 5: Bounds verification
        if let Operation::Absolute(pt) = operation
            && !self.bounds.contains(*pt)
        {
            return Err(SubmissionRefusal::OutOfBounds);
        }

        // Check 6: Update remote held state tracker
        self.held_state.record_injected_operation(operation);
        Ok(())
    }

    /// Handle worker crash or hang.
    ///
    /// Produces an honest report of uncertain release states without inventing certainty.
    /// Revokes authority and immediately drops sleep inhibitor assertions.
    pub fn on_worker_crash(&mut self, now: HostInstant) -> UncertainReleaseReport {
        self.revoked.store(true, Ordering::Release);
        let report = self.held_state.record_worker_crash();
        let _ = self.sleep_inhibitor.emergency_release_all(now);
        let _ = self
            .indicator
            .immediate_revoke(now, StopReason::NativeFailure);
        report
    }

    /// Handle OS session change / fast user switching.
    /// Authority must be revoked and never inherited.
    pub fn on_os_session_changed(&mut self, new_os_session_id: u32, now: HostInstant) {
        let _ = self.permissions.on_session_transition(new_os_session_id);
        self.immediate_revoke(now, StopReason::AuthorityEnded);
    }

    /// Handle OS screen lock. Authority must be revoked.
    pub fn on_os_locked(&mut self, now: HostInstant) {
        self.permissions.on_session_locked();
        self.immediate_revoke(now, StopReason::Suspended);
    }

    /// Handle requesting session termination.
    pub fn on_session_ended(
        &mut self,
        session_id: RemoteSessionId,
        now: HostInstant,
    ) -> Vec<Operation> {
        self.approval.on_session_ended(session_id);
        let _ = self.sleep_inhibitor.release(session_id, now);
        self.indicator.hide();
        self.held_state.synthesize_cleanup_releases()
    }
}
