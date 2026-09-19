//! Active session idle sleep inhibitor (plan §§5.3, 19.1).
//!
//! Holds the platform idle-sleep inhibitor (`IOKit` assertion on macOS, `SetThreadExecutionState`
//! on Windows, systemd/logind inhibitor on Linux) while at least one session is actively shared.
//! It prevents idle sleep ONLY — it never defeats lid-close policy or a deliberate local lock,
//! and it releases immediately when the last session ends or on worker crash.

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

/// In-memory simulated platform backend for testing and platforms without native bindings.
#[derive(Default)]
pub struct SimulatedInhibitorPlatform {
    inhibited: bool,
}

impl SimulatedInhibitorPlatform {
    pub fn new() -> Self {
        Self::default()
    }
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

/// Coordinates idle sleep inhibition across active shared sessions.
pub struct SleepInhibitor {
    platform: Box<dyn SleepInhibitorPlatform>,
    active_sessions: HashSet<RemoteSessionId>,
    logs: Vec<InhibitorLogEntry>,
    enabled: bool,
}

impl Default for SleepInhibitor {
    fn default() -> Self {
        Self::new(Box::new(SimulatedInhibitorPlatform::new()), true)
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
            self.platform.set_inhibited(true)?;
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
