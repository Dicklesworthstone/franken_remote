//! Always-visible sharing indicator with immediate synchronous revoke (plan §§5.3, 7.3, 15.2).
//!
//! Revoke is synchronous at the authority decision point; OS cleanup completion is
//! reported separately. Revoke latency is measured in nanoseconds and logged.

use crate::input_watchdog::{Control as InputControl, StopReason};
use fr_core::{ids::RemoteSessionId, input_submission::RevokeHandle, time::HostInstant};
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
    /// Revoked locally. Synchronous authority fence is complete;
    /// OS native cleanup may still be in progress.
    Revoked {
        latency_ns: u64,
        at: HostInstant,
        os_cleanup_complete: bool,
    },
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

/// Manages the always-visible local indicator and immediate revocation.
pub struct SharingIndicator {
    state: Mutex<IndicatorDisplayState>,
    targets: Mutex<Vec<RevokeTarget>>,
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
            targets: Mutex::new(Vec::new()),
            os_cleanup: Mutex::new(OsCleanupTracker::new()),
            last_revoke: Mutex::new(None),
        }
    }

    /// Register an `InputControl` to be synchronously stopped on revoke.
    pub fn register_input_control(&self, control: InputControl) {
        let mut targets = self
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        targets.push(RevokeTarget::InputControl(control));
    }

    /// Register a `RevokeHandle` to be synchronously revoked.
    pub fn register_revoke_handle(&self, handle: RevokeHandle) {
        let mut targets = self
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        targets.push(RevokeTarget::RevokeHandle(handle));
    }

    /// Register a custom revocation closure.
    pub fn register_custom_revoker(&self, revoker: impl Fn() + Send + Sync + 'static) {
        let mut targets = self
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        targets.push(RevokeTarget::Custom(Box::new(revoker)));
    }

    /// Update indicator to show an observing session.
    pub fn show_observing(
        &self,
        session_id: RemoteSessionId,
        peer_name: impl Into<String>,
        display_count: u32,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = IndicatorDisplayState::Observing {
            session_id,
            peer_name: peer_name.into(),
            display_count,
        };
    }

    /// Update indicator to show a controlling session with input lease.
    pub fn show_controlling(
        &self,
        session_id: RemoteSessionId,
        peer_name: impl Into<String>,
        display_count: u32,
        has_input_lease: bool,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = IndicatorDisplayState::Controlling {
            session_id,
            peer_name: peer_name.into(),
            display_count,
            has_input_lease,
        };
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
            IndicatorDisplayState::Observing { .. } | IndicatorDisplayState::Controlling { .. }
        )
    }

    /// Synchronously execute immediate revocation at the authority decision point.
    ///
    /// The timer measures the synchronous fence latency down to the nanosecond.
    /// Authority is guaranteed revoked before this call returns.
    /// OS-level cleanup is reported separately via `OsCleanupTracker`.
    pub fn immediate_revoke(&self, now: HostInstant, reason: StopReason) -> ImmediateRevokeOutcome {
        let t0 = Instant::now();

        // 1. Synchronously execute all registered authority revocations.
        // We take targets out of the lock so we don't hold the lock during execution.
        let targets: Vec<RevokeTarget> = {
            let mut guard = self
                .targets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *guard)
        };

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
