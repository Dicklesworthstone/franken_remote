//! Local input priority and lease suspension (plan §§5.3, 7.3, 15.2).
//!
//! Suspends the remote lease when local physical input is detected, ONLY where
//! injected vs local events are distinguishable by the OS.
//! Where not distinguishable, presents the limitation honestly instead of enabling
//! an unstable heuristic that could cause injected events to suspend their own lease.

use fr_core::time::HostInstant;
use std::time::Duration;

/// Default duration to suspend remote input after detecting physical local input.
pub const DEFAULT_SUSPENSION_DURATION: Duration = Duration::from_secs(3);

/// Whether the OS platform provides an API to distinguish injected synthetic events from physical local input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distinguishability {
    /// The OS reliably distinguishes synthetic events (e.g. via device origin or event flags).
    Distinguishable,
    /// The OS merges synthetic and physical input into the same queue without origin tags.
    Indistinguishable,
}

/// Outcome of detecting input activity under local priority policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalPriorityOutcome {
    /// Local priority is disabled. Remote lease remains unaffected.
    Disabled,
    /// Platform cannot distinguish local vs synthetic input; limitation presented honestly.
    UnsupportedPlatformHeuristic,
    /// Genuine local input detected on a distinguishable platform; remote lease suspended.
    Suspended {
        until: HostInstant,
    },
}

/// Refusal reason when remote injection is rejected due to active local priority suspension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPrioritySuspended {
    pub suspended_until: HostInstant,
}

impl core::fmt::Display for LocalPrioritySuspended {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Remote input suspended due to active local user activity")
    }
}

impl std::error::Error for LocalPrioritySuspended {}

/// Configuration for local input priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPriorityConfig {
    pub enabled: bool,
    pub distinguishability: Distinguishability,
    pub suspension_duration: Duration,
}

impl Default for LocalPriorityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // Default safe assessment: commodity OS queues are indistinguishable unless qualified
            distinguishability: Distinguishability::Indistinguishable,
            suspension_duration: DEFAULT_SUSPENSION_DURATION,
        }
    }
}

/// Coordinates local input priority and lease suspension.
pub struct LocalInputPriority {
    config: LocalPriorityConfig,
    suspended_until: Option<HostInstant>,
}

impl Default for LocalInputPriority {
    fn default() -> Self {
        Self::new(LocalPriorityConfig::default())
    }
}

impl LocalInputPriority {
    pub fn new(config: LocalPriorityConfig) -> Self {
        Self {
            config,
            suspended_until: None,
        }
    }

    pub fn config(&self) -> &LocalPriorityConfig {
        &self.config
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
        if !enabled {
            self.suspended_until = None;
        }
    }

    pub fn set_distinguishability(&mut self, dist: Distinguishability) {
        self.config.distinguishability = dist;
    }

    /// Check if remote input is currently suspended by local activity.
    pub fn is_suspended(&self, now: HostInstant) -> bool {
        if !self.config.enabled {
            return false;
        }
        self.suspended_until.is_some_and(|until| now < until)
    }

    /// Submission checkpoint check: returns Err if remote injection is suspended.
    pub fn verify_submission_allowed(&self, now: HostInstant) -> Result<(), LocalPrioritySuspended> {
        if let Some(until) = self.suspended_until {
            if now < until {
                return Err(LocalPrioritySuspended {
                    suspended_until: until,
                });
            }
        }
        Ok(())
    }

    /// Invoked when local input is detected.
    ///
    /// - If disabled: returns `Disabled`.
    /// - If platform cannot distinguish injected vs physical: returns `UnsupportedPlatformHeuristic`.
    /// - If distinguishable: suspends remote lease until `now + suspension_duration`.
    pub fn on_local_input_detected(&mut self, now: HostInstant) -> LocalPriorityOutcome {
        if !self.config.enabled {
            return LocalPriorityOutcome::Disabled;
        }

        match self.config.distinguishability {
            Distinguishability::Indistinguishable => {
                // Honest limitation report: refuse to enable unstable heuristic!
                LocalPriorityOutcome::UnsupportedPlatformHeuristic
            }
            Distinguishability::Distinguishable => {
                let dur_nanos = self.config.suspension_duration.as_nanos();
                let until = now.add_nanos(dur_nanos.min(u128::from(u64::MAX)) as u64);
                self.suspended_until = Some(until);
                LocalPriorityOutcome::Suspended { until }
            }
        }
    }

    /// Clear any active suspension immediately (e.g. on manual override or session end).
    pub fn clear_suspension(&mut self) {
        self.suspended_until = None;
    }
}
