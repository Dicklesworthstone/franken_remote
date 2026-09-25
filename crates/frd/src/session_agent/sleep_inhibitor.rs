//! Active session idle sleep inhibitor (plan §§5.3, 19.1).
//!
//! Holds the platform idle-sleep inhibitor (`IOKit` assertion on macOS, `SetThreadExecutionState`
//! on Windows, systemd/logind inhibitor on Linux) while at least one session is actively shared.
//! It prevents idle sleep ONLY — it never defeats lid-close policy or a deliberate local lock,
//! and it releases immediately when the last session ends or on worker crash.
//!
//! No native backend is implemented yet. The default platform is
//! [`UnavailableInhibitorPlatform`]: every assertion attempt is refused with
//! `sleep_inhibitor_unavailable` and the inhibitor never reports that it holds
//! an assertion the OS was never given.

use fr_core::{ids::RemoteSessionId, time::HostInstant};
use std::collections::HashSet;

/// Error returned by platform sleep inhibition operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InhibitorError {
    PlatformUnavailable,
    AlreadyAcquired,
    NotAcquired,
    LockFailed,
}

impl InhibitorError {
    /// Stable machine-readable refusal code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::PlatformUnavailable => "sleep_inhibitor_unavailable",
            Self::AlreadyAcquired => "sleep_inhibitor_already_acquired",
            Self::NotAcquired => "sleep_inhibitor_not_acquired",
            Self::LockFailed => "sleep_inhibitor_lock_failed",
        }
    }
}

impl core::fmt::Display for InhibitorError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PlatformUnavailable => write!(f, "Platform sleep inhibition unavailable"),
            Self::AlreadyAcquired => write!(f, "Inhibitor assertion already held for session"),
            Self::NotAcquired => write!(f, "Inhibitor assertion not held for session"),
            Self::LockFailed => write!(f, "Failed to acquire inhibitor lock"),
        }
    }
}

impl std::error::Error for InhibitorError {}

/// Action performed on the sleep inhibitor for auditing and log verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InhibitorAction {
    Acquired,
    Released,
    EmergencyReleasedAll,
}

/// Recorded event log entry for inhibitor transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InhibitorLogEntry {
    pub session_id: Option<RemoteSessionId>,
    pub action: InhibitorAction,
    pub active_sessions_count: usize,
    pub at: HostInstant,
}

/// Platform-specific sleep inhibition backend contract.
pub trait SleepInhibitorPlatform: Send + Sync {
    /// Request the OS to inhibit or allow idle sleep.
    fn set_inhibited(&mut self, inhibited: bool) -> Result<(), InhibitorError>;
    /// Check whether the platform inhibitor is currently asserted.
    fn is_inhibited(&self) -> bool;
}

/// The platform used until a native backend (logind `Inhibit`, `IOKit`,
/// `SetThreadExecutionState`) exists: it never asserts anything and refuses
/// every assertion with [`InhibitorError::PlatformUnavailable`].
#[derive(Debug, Default)]
pub struct UnavailableInhibitorPlatform;

impl SleepInhibitorPlatform for UnavailableInhibitorPlatform {
    fn set_inhibited(&mut self, inhibited: bool) -> Result<(), InhibitorError> {
        if inhibited {
            Err(InhibitorError::PlatformUnavailable)
        } else {
            // Nothing is held, so there is nothing to release.
            Ok(())
        }
    }

    fn is_inhibited(&self) -> bool {
        false
    }
}

/// Coordinates idle sleep inhibition across active shared sessions.
pub struct SleepInhibitor {
    platform: Box<dyn SleepInhibitorPlatform>,
    active_sessions: HashSet<RemoteSessionId>,
    logs: Vec<InhibitorLogEntry>,
    enabled: bool,
}

impl Default for SleepInhibitor {
    /// Enabled, over [`UnavailableInhibitorPlatform`]: acquisition is a typed
    /// `sleep_inhibitor_unavailable` refusal, never a simulated assertion.
    fn default() -> Self {
        Self::new(Box::new(UnavailableInhibitorPlatform), true)
    }
}

impl SleepInhibitor {
    pub fn new(platform: Box<dyn SleepInhibitorPlatform>, enabled: bool) -> Self {
        Self {
            platform,
            active_sessions: HashSet::new(),
            logs: Vec::new(),
            enabled,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool, now: HostInstant) -> Result<(), InhibitorError> {
        if self.enabled != enabled {
            self.enabled = enabled;
            if !enabled && self.platform.is_inhibited() {
                self.platform.set_inhibited(false)?;
                self.logs.push(InhibitorLogEntry {
                    session_id: None,
                    action: InhibitorAction::Released,
                    active_sessions_count: self.active_sessions.len(),
                    at: now,
                });
            } else if enabled && !self.active_sessions.is_empty() && !self.platform.is_inhibited() {
                self.platform.set_inhibited(true)?;
                self.logs.push(InhibitorLogEntry {
                    session_id: None,
                    action: InhibitorAction::Acquired,
                    active_sessions_count: self.active_sessions.len(),
                    at: now,
                });
            }
        }
        Ok(())
    }

    /// Number of sessions currently holding the inhibitor.
    pub fn active_session_count(&self) -> usize {
        self.active_sessions.len()
    }

    /// True if the platform inhibitor assertion is actively held.
    pub fn is_inhibiting(&self) -> bool {
        self.platform.is_inhibited()
    }

    /// Retained audit logs of inhibitor transitions.
    pub fn logs(&self) -> &[InhibitorLogEntry] {
        &self.logs
    }

    /// Acquire the sleep inhibitor for an active shared session.
    /// If this is the first session, activates the OS assertion.
    pub fn acquire(
        &mut self,
        session_id: RemoteSessionId,
        now: HostInstant,
    ) -> Result<bool, InhibitorError> {
        if !self.enabled {
            return Ok(false);
        }

        let was_empty = self.active_sessions.is_empty();
        self.active_sessions.insert(session_id);

        let newly_inhibited = if was_empty {
            if let Err(error) = self.platform.set_inhibited(true) {
                // The OS holds nothing: the session does not hold the inhibitor,
                // and no `Acquired` entry is logged for an assertion that failed.
                self.active_sessions.remove(&session_id);
                return Err(error);
            }
            true
        } else {
            false
        };

        self.logs.push(InhibitorLogEntry {
            session_id: Some(session_id),
            action: InhibitorAction::Acquired,
            active_sessions_count: self.active_sessions.len(),
            at: now,
        });

        Ok(newly_inhibited)
    }

    /// Release the sleep inhibitor for a session that has ended.
    /// If no active sessions remain, immediately drops the OS assertion.
    pub fn release(
        &mut self,
        session_id: RemoteSessionId,
        now: HostInstant,
    ) -> Result<bool, InhibitorError> {
        let removed = self.active_sessions.remove(&session_id);
        if !removed {
            return Ok(false);
        }

        let dropped = if self.active_sessions.is_empty() && self.platform.is_inhibited() {
            self.platform.set_inhibited(false)?;
            true
        } else {
            false
        };

        self.logs.push(InhibitorLogEntry {
            session_id: Some(session_id),
            action: InhibitorAction::Released,
            active_sessions_count: self.active_sessions.len(),
            at: now,
        });

        Ok(dropped)
    }

    /// Immediately release all held inhibitor assertions (e.g. on crash, logout, or revoke).
    pub fn emergency_release_all(&mut self, now: HostInstant) -> Result<bool, InhibitorError> {
        let had_sessions = !self.active_sessions.is_empty();
        self.active_sessions.clear();

        let dropped = if self.platform.is_inhibited() {
            self.platform.set_inhibited(false)?;
            true
        } else {
            false
        };

        if had_sessions || dropped {
            self.logs.push(InhibitorLogEntry {
                session_id: None,
                action: InhibitorAction::EmergencyReleasedAll,
                active_sessions_count: 0,
                at: now,
            });
        }

        Ok(dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only in-memory platform; it asserts nothing to any OS.
    #[derive(Default)]
    struct SimulatedInhibitorPlatform {
        inhibited: bool,
    }

    impl SleepInhibitorPlatform for SimulatedInhibitorPlatform {
        fn set_inhibited(&mut self, inhibited: bool) -> Result<(), InhibitorError> {
            self.inhibited = inhibited;
            Ok(())
        }

        fn is_inhibited(&self) -> bool {
            self.inhibited
        }
    }

    const T0: HostInstant = HostInstant::from_micros(1_000_000);

    #[test]
    fn default_inhibitor_refuses_with_sleep_inhibitor_unavailable() {
        let mut inhibitor = SleepInhibitor::default();
        let session = RemoteSessionId::from_raw(1);

        assert_eq!(
            inhibitor.acquire(session, T0),
            Err(InhibitorError::PlatformUnavailable)
        );
        assert_eq!(
            InhibitorError::PlatformUnavailable.code(),
            "sleep_inhibitor_unavailable"
        );
        // Nothing was asserted, so nothing is claimed as held or logged.
        assert!(!inhibitor.is_inhibiting());
        assert_eq!(inhibitor.active_session_count(), 0);
        assert_eq!(inhibitor.logs(), []);

        // A second session is refused the same way instead of piggybacking on
        // an assertion that never existed.
        assert_eq!(
            inhibitor.acquire(RemoteSessionId::from_raw(2), T0),
            Err(InhibitorError::PlatformUnavailable)
        );
        assert_eq!(inhibitor.active_session_count(), 0);

        // Releasing and emergency release stay harmless with nothing held.
        assert_eq!(inhibitor.release(session, T0), Ok(false));
        assert_eq!(inhibitor.emergency_release_all(T0), Ok(false));
        assert_eq!(inhibitor.logs(), []);
    }

    #[test]
    fn sleep_inhibitor_ref_counts_over_an_explicit_platform() {
        let mut inhibitor = SleepInhibitor::new(Box::<SimulatedInhibitorPlatform>::default(), true);
        let first = RemoteSessionId::from_raw(1);
        let second = RemoteSessionId::from_raw(2);

        assert_eq!(inhibitor.acquire(first, T0), Ok(true));
        assert_eq!(inhibitor.acquire(second, T0), Ok(false));
        assert!(inhibitor.is_inhibiting());
        assert_eq!(inhibitor.release(first, T0), Ok(false));
        assert!(inhibitor.is_inhibiting());
        assert_eq!(inhibitor.release(second, T0), Ok(true));
        assert!(!inhibitor.is_inhibiting());
    }
}
