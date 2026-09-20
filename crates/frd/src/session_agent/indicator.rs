//! Always-visible sharing indicator with immediate synchronous revoke (plan §§5.3, 7.3, 15.2).
//!
//! Revoke is synchronous at the authority decision point; OS cleanup completion is
//! reported separately. Revoke latency is measured in nanoseconds and logged.

use super::{GrantedScope, SessionRole};
use crate::input_watchdog::{Control as InputControl, StopReason};
use fr_core::{ids::RemoteSessionId, input_submission::RevokeHandle, time::HostInstant};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

/// Current display and sharing state of the local indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndicatorDisplayState {
    /// No session active, indicator hidden.
    Hidden,
    /// Active read-only observer session visible to user.
    Observing {
        session_id: RemoteSessionId,
        peer_name: String,
        display_count: u32,
    },
    /// Active controller session with input lease visible to user.
    Controlling {
        session_id: RemoteSessionId,
        peer_name: String,
        display_count: u32,
        has_input_lease: bool,
    },
    /// Multiple connected sessions active concurrently.
    ActiveSessions { sessions: Vec<ConnectedSession> },
    /// Revoked locally. Synchronous authority fence is complete;
    /// OS native cleanup may still be in progress.
    Revoked {
        latency_ns: u64,
        at: HostInstant,
        os_cleanup_complete: bool,
    },
}

/// Detailed view of a connected session in the session agent UI (plan §§2.2, 7.1, 15.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedSession {
    pub session_id: RemoteSessionId,
    pub device_name: String,
    pub role: SessionRole,
    pub capabilities: SessionCapabilitiesInUse,
    pub connected_at: HostInstant,
}

/// Capabilities currently in use by a connected session:
/// view, control, audio, clipboard, files (plan §15.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SessionCapabilitiesInUse {
    pub view: bool,
    pub control: bool,
    pub audio: bool,
    pub clipboard: bool,
    pub files: bool,
}

impl SessionCapabilitiesInUse {
    pub fn from_granted_scope(scope: &GrantedScope) -> Self {
        Self {
            view: !scope.displays.is_empty(),
            control: scope.role == SessionRole::Controller,
            audio: scope.audio != super::AudioScope::None,
            clipboard: scope.clipboard,
            files: scope.file_transfer,
        }
    }
}

/// Asynchronous tracker for OS-level cleanup (releasing native window/cursor/portal resources).
/// This is reported separately from the sub-microsecond authority revocation fence.
#[derive(Clone, Default)]
pub struct OsCleanupTracker {
    complete: Arc<AtomicBool>,
}

impl OsCleanupTracker {
    pub fn new() -> Self {
        Self {
            complete: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn mark_complete(&self) {
        self.complete.store(true, Ordering::Release);
    }

    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }
}

/// The outcome of an immediate revocation request.
#[derive(Clone)]
pub struct ImmediateRevokeOutcome {
    /// Duration of the synchronous authority fence in nanoseconds.
    pub latency_ns: u64,
    /// Host timestamp when revoke completed.
    pub at: HostInstant,
    /// Handle to track asynchronous completion of native OS cleanup.
    pub os_cleanup: OsCleanupTracker,
}

impl core::fmt::Debug for ImmediateRevokeOutcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImmediateRevokeOutcome")
            .field("latency_ns", &self.latency_ns)
            .field("at", &self.at)
            .field("os_cleanup_complete", &self.os_cleanup.is_complete())
            .finish()
    }
}

/// Registration handle for revoking authority.
enum RevokeTarget {
    InputControl(InputControl),
    RevokeHandle(RevokeHandle),
    Custom(Box<dyn Fn() + Send + Sync + 'static>),
}

/// Manages the always-visible local indicator, connected sessions list, and immediate revocation.
pub struct SharingIndicator {
    state: Mutex<IndicatorDisplayState>,
    sessions: Mutex<HashMap<RemoteSessionId, ConnectedSession>>,
    targets: Mutex<Vec<RevokeTarget>>,
    session_targets: Mutex<HashMap<RemoteSessionId, Vec<RevokeTarget>>>,
    os_cleanup: Mutex<OsCleanupTracker>,
    last_revoke: Mutex<Option<ImmediateRevokeOutcome>>,
}

impl Default for SharingIndicator {
    fn default() -> Self {
        Self::new()
    }
}

impl SharingIndicator {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(IndicatorDisplayState::Hidden),
            sessions: Mutex::new(HashMap::new()),
            targets: Mutex::new(Vec::new()),
            session_targets: Mutex::new(HashMap::new()),
            os_cleanup: Mutex::new(OsCleanupTracker::new()),
            last_revoke: Mutex::new(None),
        }
    }

    /// Register a global `InputControl` to be synchronously stopped on revoke.
    pub fn register_input_control(&self, control: InputControl) {
        let mut targets = self
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        targets.push(RevokeTarget::InputControl(control));
    }

    /// Register a global `RevokeHandle` to be synchronously revoked.
    pub fn register_revoke_handle(&self, handle: RevokeHandle) {
        let mut targets = self
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        targets.push(RevokeTarget::RevokeHandle(handle));
    }

    /// Register a global custom revocation closure.
    pub fn register_custom_revoker(&self, revoker: impl Fn() + Send + Sync + 'static) {
        let mut targets = self
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        targets.push(RevokeTarget::Custom(Box::new(revoker)));
    }

    /// Register an `InputControl` scoped to a specific session.
    pub fn register_session_input_control(
        &self,
        session_id: RemoteSessionId,
        control: InputControl,
    ) {
        let mut targets = self
            .session_targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        targets
            .entry(session_id)
            .or_default()
            .push(RevokeTarget::InputControl(control));
    }

    /// Register a `RevokeHandle` scoped to a specific session.
    pub fn register_session_revoke_handle(
        &self,
        session_id: RemoteSessionId,
        handle: RevokeHandle,
    ) {
        let mut targets = self
            .session_targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        targets
            .entry(session_id)
            .or_default()
            .push(RevokeTarget::RevokeHandle(handle));
    }

    /// Register a custom revocation closure scoped to a specific session.
    pub fn register_session_custom_revoker(
        &self,
        session_id: RemoteSessionId,
        revoker: impl Fn() + Send + Sync + 'static,
    ) {
        let mut targets = self
            .session_targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        targets
            .entry(session_id)
            .or_default()
            .push(RevokeTarget::Custom(Box::new(revoker)));
    }

    /// Add or update a connected session in the UI.
    pub fn add_connected_session(&self, session: ConnectedSession) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.insert(session.session_id, session);
        self.recompute_display_state(&sessions);
    }

    /// Returns a list of all currently connected sessions sorted by session ID.
    pub fn connected_sessions(&self) -> Vec<ConnectedSession> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut list: Vec<ConnectedSession> = sessions.values().cloned().collect();
        list.sort_by_key(|s| s.session_id.as_raw());
        list
    }

    /// Returns a specific connected session by its ID.
    pub fn connected_session(&self, session_id: RemoteSessionId) -> Option<ConnectedSession> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.get(&session_id).cloned()
    }

    /// Update capabilities in use for an active connected session.
    pub fn update_session_capabilities(
        &self,
        session_id: RemoteSessionId,
        capabilities: SessionCapabilitiesInUse,
    ) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(session) = sessions.get_mut(&session_id) {
            session.capabilities = capabilities;
            self.recompute_display_state(&sessions);
        }
    }

    /// Remove a connected session from the UI without full revocation.
    pub fn remove_session(&self, session_id: RemoteSessionId) -> Option<ConnectedSession> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = sessions.remove(&session_id);
        self.recompute_display_state(&sessions);
        removed
    }

    /// Recomputes display state from active sessions map.
    fn recompute_display_state(&self, sessions: &HashMap<RemoteSessionId, ConnectedSession>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if sessions.is_empty() {
            if !matches!(*state, IndicatorDisplayState::Revoked { .. }) {
                *state = IndicatorDisplayState::Hidden;
            }
        } else if sessions.len() == 1 {
            let s = sessions.values().next().unwrap();
            let display_count = u32::from(s.capabilities.view);
            match s.role {
                SessionRole::Controller => {
                    *state = IndicatorDisplayState::Controlling {
                        session_id: s.session_id,
                        peer_name: s.device_name.clone(),
                        display_count,
                        has_input_lease: s.capabilities.control,
                    };
                }
                SessionRole::Observer => {
                    *state = IndicatorDisplayState::Observing {
                        session_id: s.session_id,
                        peer_name: s.device_name.clone(),
                        display_count,
                    };
                }
            }
        } else {
            let mut list: Vec<ConnectedSession> = sessions.values().cloned().collect();
            list.sort_by_key(|s| s.session_id.as_raw());
            *state = IndicatorDisplayState::ActiveSessions { sessions: list };
        }
    }

    /// Update indicator to show an observing session.
    pub fn show_observing(
        &self,
        session_id: RemoteSessionId,
        peer_name: impl Into<String>,
        display_count: u32,
    ) {
        let peer_name = peer_name.into();
        let conn = ConnectedSession {
            session_id,
            device_name: peer_name,
            role: SessionRole::Observer,
            capabilities: SessionCapabilitiesInUse {
                view: display_count > 0,
                control: false,
                audio: false,
                clipboard: false,
                files: false,
            },
            connected_at: HostInstant::ORIGIN,
        };
        self.add_connected_session(conn);
    }

    /// Update indicator to show a controlling session with input lease.
    pub fn show_controlling(
        &self,
        session_id: RemoteSessionId,
        peer_name: impl Into<String>,
        display_count: u32,
        has_input_lease: bool,
    ) {
        let peer_name = peer_name.into();
        let conn = ConnectedSession {
            session_id,
            device_name: peer_name,
            role: SessionRole::Controller,
            capabilities: SessionCapabilitiesInUse {
                view: display_count > 0,
                control: has_input_lease,
                audio: false,
                clipboard: false,
                files: false,
            },
            connected_at: HostInstant::ORIGIN,
        };
        self.add_connected_session(conn);
    }

    /// Return the current visual indicator display state.
    pub fn display_state(&self) -> IndicatorDisplayState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Is any session currently visibly indicated as active?
    pub fn is_active(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches!(
            *state,
            IndicatorDisplayState::Observing { .. }
                | IndicatorDisplayState::Controlling { .. }
                | IndicatorDisplayState::ActiveSessions { .. }
        )
    }

    /// Synchronously revoke authority for ONE specific connected session.
    ///
    /// Other connected sessions remain unaffected.
    /// Returns the immediate revocation outcome if the session existed and was revoked.
    pub fn revoke_session(
        &self,
        session_id: RemoteSessionId,
        now: HostInstant,
        reason: StopReason,
    ) -> Option<ImmediateRevokeOutcome> {
        let t0 = Instant::now();

        // Take targets for this session
        let targets = {
            let mut guard = self
                .session_targets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.remove(&session_id)
        };

        // Remove session from active sessions
        let session_existed = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let existed = sessions.remove(&session_id).is_some();
            self.recompute_display_state(&sessions);
            existed
        };

        if targets.is_none() && !session_existed {
            return None;
        }

        if let Some(targets) = targets {
            for target in targets {
                match target {
                    RevokeTarget::InputControl(ctrl) => {
                        ctrl.stop(reason);
                    }
                    RevokeTarget::RevokeHandle(handle) => {
                        handle.revoke();
                    }
                    RevokeTarget::Custom(f) => {
                        f();
                    }
                }
            }
        }

        let elapsed = t0.elapsed();
        let latency_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);

        let cleanup_tracker = OsCleanupTracker::new();
        *self
            .os_cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cleanup_tracker.clone();

        // If all sessions are now gone, transition display state to Revoked
        {
            let sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if sessions.is_empty() {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *state = IndicatorDisplayState::Revoked {
                    latency_ns,
                    at: now,
                    os_cleanup_complete: false,
                };
            }
        }

        let outcome = ImmediateRevokeOutcome {
            latency_ns,
            at: now,
            os_cleanup: cleanup_tracker,
        };

        let mut last = self
            .last_revoke
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *last = Some(outcome.clone());

        Some(outcome)
    }

    /// Synchronously execute immediate revocation across ALL sessions at the authority decision point.
    ///
    /// The timer measures the synchronous fence latency down to the nanosecond.
    /// Authority is guaranteed revoked before this call returns.
    /// OS-level cleanup is reported separately via `OsCleanupTracker`.
    pub fn immediate_revoke(&self, now: HostInstant, reason: StopReason) -> ImmediateRevokeOutcome {
        let t0 = Instant::now();

        // 1. Synchronously execute all registered authority revocations (global and per-session).
        let global_targets: Vec<RevokeTarget> = {
            let mut guard = self
                .targets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *guard)
        };

        let session_targets_map: HashMap<RemoteSessionId, Vec<RevokeTarget>> = {
            let mut guard = self
                .session_targets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *guard)
        };

        // Clear active sessions
        {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sessions.clear();
        }

        for target in global_targets {
            match target {
                RevokeTarget::InputControl(ctrl) => {
                    ctrl.stop(reason);
                }
                RevokeTarget::RevokeHandle(handle) => {
                    handle.revoke();
                }
                RevokeTarget::Custom(f) => {
                    f();
                }
            }
        }

        for (_, targets) in session_targets_map {
            for target in targets {
                match target {
                    RevokeTarget::InputControl(ctrl) => {
                        ctrl.stop(reason);
                    }
                    RevokeTarget::RevokeHandle(handle) => {
                        handle.revoke();
                    }
                    RevokeTarget::Custom(f) => {
                        f();
                    }
                }
            }
        }

        // Measure synchronous elapsed time
        let elapsed = t0.elapsed();
        let latency_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);

        // Reset cleanup tracker for the new teardown
        let cleanup_tracker = OsCleanupTracker::new();
        *self
            .os_cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cleanup_tracker.clone();

        // Update indicator display state
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *state = IndicatorDisplayState::Revoked {
                latency_ns,
                at: now,
                os_cleanup_complete: false,
            };
        }

        let outcome = ImmediateRevokeOutcome {
            latency_ns,
            at: now,
            os_cleanup: cleanup_tracker,
        };

        // Record last outcome
        {
            let mut last = self
                .last_revoke
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *last = Some(outcome.clone());
        }

        outcome
    }

    /// Mark OS cleanup complete (called by native cleanup loop/worker when OS release finishes).
    pub fn mark_os_cleanup_complete(&self) {
        self.os_cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mark_complete();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let IndicatorDisplayState::Revoked {
            os_cleanup_complete,
            ..
        } = &mut *state
        {
            *os_cleanup_complete = true;
        }
    }

    /// Hide indicator once everything is reaped.
    pub fn hide(&self) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.clear();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = IndicatorDisplayState::Hidden;
    }

    /// Get last revocation outcome if any.
    pub fn last_revoke_outcome(&self) -> Option<ImmediateRevokeOutcome> {
        self.last_revoke
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
