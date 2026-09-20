#![forbid(unsafe_code)]
//! Worker lifecycle state machine and supervision (plan §§5.4, 10.2, 11.1).
//!
//! Enforces:
//! 1. Spawn on demand: Workers are instantiated only when observation/control is demanded.
//! 2. Generation-fenced teardown: Fixed strict teardown order (revoke input -> invalidate
//!    generations -> stop capture -> cancel tasks -> drain sends -> kill worker -> publish).
//! 3. Kill-on-stall with authority fencing FIRST: If a worker stalls (timeout exceeded),
//!    authority is fenced and revoked before the worker is terminated.
//! 4. Restart with bounded rate and exponential backoff.
//! 5. Repeated GPU failure ends the media profile rather than spawning unlimited workers.
//! 6. Typed capability events: permission loss, display removal, color changes, and
//!    protected content (reported as typed events, never as a stalled network).

use asupersync::types::Time;
use core::fmt;
use std::time::Duration;

/// States of the media worker lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// Worker is not running; awaiting on-demand session request.
    Idle,
    /// Worker process spawn has been initiated.
    Spawning,
    /// Worker process is running and undergoing initial protocol handshake/configuration.
    Configuring,
    /// Worker is actively running, capturing, and encoding frames.
    Running,
    /// Exchange deadline or heartbeat timeout exceeded; stall detected.
    Stalled,
    /// Kill-on-stall: input authority is being revoked and generations fenced FIRST.
    FencingAuthority,
    /// Child process is being terminated and reaped.
    Terminating,
    /// In exponential backoff delay before the next permitted restart attempt.
    Backoff,
    /// Worker profile permanently disabled for this session/daemon instance.
    ProfileDisabled(ProfileDisableReason),
}

impl WorkerState {
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    pub const fn is_disabled(&self) -> bool {
        matches!(self, Self::ProfileDisabled(_))
    }
}

/// Typed reasons for permanently disabling the media worker profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileDisableReason {
    /// Consecutive GPU device failures exceeded the configured threshold.
    RepeatedGpuFailure,
    /// Screen capture permission was permanently denied or revoked by the OS/user.
    PermissionDenied,
    /// Unsupported hardware, OS version, or missing required codec capabilities.
    Unsupported,
    /// Restart frequency exceeded the rate limit within the sliding window.
    MaxRestartsExceeded,
}

impl fmt::Display for ProfileDisableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepeatedGpuFailure => write!(f, "repeated_gpu_failure"),
            Self::PermissionDenied => write!(f, "permission_denied"),
            Self::Unsupported => write!(f, "unsupported"),
            Self::MaxRestartsExceeded => write!(f, "max_restarts_exceeded"),
        }
    }
}

/// Ordered stages of the constitutional generation-fenced teardown sequence (plan §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TeardownStage {
    /// Stage 1: Revoke input authority immediately.
    RevokeInputAuthority = 1,
    /// Stage 2: Release remotely held keys/buttons and invalidate generations.
    InvalidateGenerations = 2,
    /// Stage 3: Stop capture admission.
    StopCaptureAdmission = 3,
    /// Stage 4: Cancel cooperative tasks.
    CancelCooperativeTasks = 4,
    /// Stage 5: Drain bounded sends.
    DrainBoundedSends = 5,
    /// Stage 6: Terminate stuck foreign worker if necessary.
    TerminateWorker = 6,
    /// Stage 7: Reap process and publish closure.
    ReapWorker = 7,
    /// Teardown completely finished.
    Completed = 8,
}

/// Typed capability events emitted by the platform capture/codec adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerCapabilityEvent {
    /// Screen capture permission (e.g. macOS TCC Screen Recording) was denied or revoked.
    PermissionLoss,
    /// A display was removed or unplugged from the host.
    DisplayRemoved { display_id: u32 },
    /// Display resolution or geometry changed.
    GeometryChanged { new_width: u32, new_height: u32 },
    /// Display color space or HDR profile changed.
    DisplayColorChanged { new_color_space: String },
    /// Protected content was detected (DRM / `FairPlay` / secure window).
    /// MUST be reported as a typed event, NEVER as a stalled network!
    ProtectedContentDetected,
    /// Underlying GPU device was lost (driver reset, crash, unplug).
    GpuDeviceLost,
}

/// Configuration knobs governing worker lifecycle, restart rates, and backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerLifecycleConfig {
    /// Maximum restart attempts allowed within the sliding window.
    pub max_restarts: u32,
    /// Maximum consecutive GPU failures allowed before disabling the profile.
    pub max_gpu_failures: u32,
    /// Initial restart backoff duration.
    pub initial_backoff: Duration,
    /// Maximum backoff duration ceiling.
    pub max_backoff: Duration,
    /// Duration of the sliding window for counting restarts.
    pub restart_window: Duration,
    /// Maximum allowed duration without progress before declaring a stall.
    pub stall_timeout: Duration,
}

impl Default for WorkerLifecycleConfig {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            max_gpu_failures: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(2000),
            restart_window: Duration::from_secs(10),
            stall_timeout: Duration::from_millis(1500),
        }
    }
}

/// Errors returned by lifecycle state machine operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleError {
    /// State transition is invalid from the current state.
    InvalidTransition {
        current: WorkerState,
        attempted: &'static str,
    },
    /// The media profile is permanently disabled.
    ProfileDisabled(ProfileDisableReason),
    /// Restart backoff duration has not yet expired.
    BackoffNotExpired { remaining_ms: u64 },
    /// Authority has not been fenced before attempting termination on a stall.
    AuthorityNotFenced,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { current, attempted } => {
                write!(f, "invalid transition to {attempted} from {current:?}")
            }
            Self::ProfileDisabled(reason) => write!(f, "profile disabled: {reason}"),
            Self::BackoffNotExpired { remaining_ms } => {
                write!(f, "backoff not expired ({remaining_ms} ms remaining)")
            }
            Self::AuthorityNotFenced => {
                write!(f, "authority must be fenced FIRST before killing worker")
            }
        }
    }
}

impl std::error::Error for LifecycleError {}

/// State machine governing media worker process lifecycle, authority fencing,
/// and restart policies.
#[derive(Debug)]
pub struct WorkerLifecycleStateMachine {
    state: WorkerState,
    config: WorkerLifecycleConfig,
    generation: u64,
    consecutive_gpu_failures: u32,
    restart_history: Vec<Time>,
    last_activity: Option<Time>,
    backoff_until: Option<Time>,
    backoff_attempt: u32,
    authority_fenced: bool,
    input_revoked: bool,
    protected_content_active: bool,
    teardown_log: Vec<TeardownStage>,
}

impl WorkerLifecycleStateMachine {
    /// Initialize a new lifecycle state machine starting in `Idle`.
    #[must_use]
    pub fn new(config: WorkerLifecycleConfig, initial_generation: u64) -> Self {
        Self {
            state: WorkerState::Idle,
            config,
            generation: initial_generation,
            consecutive_gpu_failures: 0,
            restart_history: Vec::new(),
            last_activity: None,
            backoff_until: None,
            backoff_attempt: 0,
            authority_fenced: false,
            input_revoked: false,
            protected_content_active: false,
            teardown_log: Vec::new(),
        }
    }

    pub const fn state(&self) -> WorkerState {
        self.state
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn consecutive_gpu_failures(&self) -> u32 {
        self.consecutive_gpu_failures
    }

    pub const fn is_authority_fenced(&self) -> bool {
        self.authority_fenced
    }

    pub const fn is_input_revoked(&self) -> bool {
        self.input_revoked
    }

    pub const fn is_protected_content_active(&self) -> bool {
        self.protected_content_active
    }

    pub fn teardown_log(&self) -> &[TeardownStage] {
        &self.teardown_log
    }

    /// Prune restart history timestamps outside the sliding window.
    fn prune_restart_history(&mut self, now: Time) {
        let window_nanos = u64::try_from(self.config.restart_window.as_nanos()).unwrap_or(u64::MAX);
        let now_nanos = now.as_nanos();
        self.restart_history
            .retain(|t| now_nanos.saturating_sub(t.as_nanos()) <= window_nanos);
    }

    /// Calculate the exponential backoff duration for the current attempt.
    #[must_use]
    pub fn compute_backoff(&self) -> Duration {
        if self.backoff_attempt == 0 {
            return self.config.initial_backoff;
        }
        let shift = self.backoff_attempt.min(6);
        let multiplier = 1u64 << shift;
        let base_nanos = u64::try_from(self.config.initial_backoff.as_nanos()).unwrap_or(u64::MAX);
        let nanos = base_nanos.saturating_mul(multiplier);
        Duration::from_nanos(nanos).min(self.config.max_backoff)
    }

    /// Request spawning the media worker on demand.
    ///
    /// Transitions from `Idle` or `Backoff` (if expired) to `Spawning`.
    /// Advances the generation counter to fence off any prior worker instance.
    pub fn request_spawn(&mut self, now: Time) -> Result<u64, LifecycleError> {
        if let WorkerState::ProfileDisabled(reason) = self.state {
            return Err(LifecycleError::ProfileDisabled(reason));
        }

        if self.state == WorkerState::Backoff {
            if let Some(until) = self.backoff_until
                && now < until
            {
                let diff_nanos = until.as_nanos().saturating_sub(now.as_nanos());
                let remaining_ms = diff_nanos / 1_000_000;
                return Err(LifecycleError::BackoffNotExpired { remaining_ms });
            }
        } else if self.state != WorkerState::Idle {
            return Err(LifecycleError::InvalidTransition {
                current: self.state,
                attempted: "Spawning",
            });
        }

        self.prune_restart_history(now);
        if self.restart_history.len() >= self.config.max_restarts as usize {
            self.state = WorkerState::ProfileDisabled(ProfileDisableReason::MaxRestartsExceeded);
            return Err(LifecycleError::ProfileDisabled(
                ProfileDisableReason::MaxRestartsExceeded,
            ));
        }

        self.generation = self.generation.wrapping_add(1);
        self.state = WorkerState::Spawning;
        self.authority_fenced = false;
        self.input_revoked = false;
        self.last_activity = Some(now);
        self.teardown_log.clear();
        Ok(self.generation)
    }

    /// Confirm successful process spawn; transitions to `Configuring`.
    pub fn on_spawn_success(&mut self) -> Result<(), LifecycleError> {
        if self.state != WorkerState::Spawning {
            return Err(LifecycleError::InvalidTransition {
                current: self.state,
                attempted: "Configuring",
            });
        }
        self.state = WorkerState::Configuring;
        Ok(())
    }

    /// Record failure during spawn or configuration exchange.
    pub fn on_spawn_failure(&mut self, now: Time, is_gpu_failure: bool) -> WorkerState {
        self.record_failure(now, is_gpu_failure)
    }

    /// Confirm worker configuration complete; transitions to `Running`.
    /// Resets the consecutive GPU failure counter and backoff attempt counter.
    pub fn on_configured(&mut self, now: Time) -> Result<(), LifecycleError> {
        if self.state != WorkerState::Configuring {
            return Err(LifecycleError::InvalidTransition {
                current: self.state,
                attempted: "Running",
            });
        }
        self.state = WorkerState::Running;
        self.last_activity = Some(now);
        self.consecutive_gpu_failures = 0;
        self.backoff_attempt = 0;
        self.backoff_until = None;
        Ok(())
    }

    /// Record forward progress / heartbeat from the running worker.
    pub fn on_progress(&mut self, now: Time) -> Result<(), LifecycleError> {
        if self.state != WorkerState::Running {
            return Err(LifecycleError::InvalidTransition {
                current: self.state,
                attempted: "Progress",
            });
        }
        self.last_activity = Some(now);
        Ok(())
    }

    /// Check if the running worker has stalled past the stall timeout.
    ///
    /// If stalled, transitions to `Stalled`.
    pub fn check_stall(&mut self, now: Time) -> bool {
        if self.state != WorkerState::Running {
            return false;
        }
        if let Some(last) = self.last_activity {
            let timeout_nanos =
                u64::try_from(self.config.stall_timeout.as_nanos()).unwrap_or(u64::MAX);
            if now.as_nanos().saturating_sub(last.as_nanos()) > timeout_nanos {
                self.state = WorkerState::Stalled;
                return true;
            }
        }
        false
    }

    /// Kill-on-stall: Fence and revoke input authority FIRST (plan §5.4).
    ///
    /// Transitions from `Stalled` to `FencingAuthority`.
    /// Revokes input authority, advances the generation, and logs teardown stages.
    pub fn fence_authority_first_on_stall(&mut self) -> Result<(), LifecycleError> {
        if self.state != WorkerState::Stalled {
            return Err(LifecycleError::InvalidTransition {
                current: self.state,
                attempted: "FencingAuthority",
            });
        }
        self.state = WorkerState::FencingAuthority;
        self.input_revoked = true;
        self.authority_fenced = true;
        self.generation = self.generation.wrapping_add(1);
        self.teardown_log.push(TeardownStage::RevokeInputAuthority);
        self.teardown_log.push(TeardownStage::InvalidateGenerations);
        Ok(())
    }

    /// Terminate and reap the worker process after authority has been fenced.
    pub fn terminate_after_fencing(
        &mut self,
        now: Time,
        is_gpu_failure: bool,
    ) -> Result<WorkerState, LifecycleError> {
        if self.state != WorkerState::FencingAuthority {
            return Err(LifecycleError::AuthorityNotFenced);
        }
        self.teardown_log.push(TeardownStage::StopCaptureAdmission);
        self.teardown_log
            .push(TeardownStage::CancelCooperativeTasks);
        self.teardown_log.push(TeardownStage::DrainBoundedSends);
        self.teardown_log.push(TeardownStage::TerminateWorker);
        self.teardown_log.push(TeardownStage::ReapWorker);
        self.teardown_log.push(TeardownStage::Completed);

        let new_state = self.record_failure(now, is_gpu_failure);
        Ok(new_state)
    }

    /// Record a failure and determine whether to back off or disable the profile.
    fn record_failure(&mut self, now: Time, is_gpu_failure: bool) -> WorkerState {
        self.restart_history.push(now);
        self.prune_restart_history(now);

        if is_gpu_failure {
            self.consecutive_gpu_failures = self.consecutive_gpu_failures.saturating_add(1);
            if self.consecutive_gpu_failures >= self.config.max_gpu_failures {
                self.state = WorkerState::ProfileDisabled(ProfileDisableReason::RepeatedGpuFailure);
                return self.state;
            }
        }

        if self.restart_history.len() >= self.config.max_restarts as usize {
            self.state = WorkerState::ProfileDisabled(ProfileDisableReason::MaxRestartsExceeded);
            return self.state;
        }

        let backoff = self.compute_backoff();
        let backoff_nanos = u64::try_from(backoff.as_nanos()).unwrap_or(u64::MAX);
        self.backoff_until = Some(Time::from_nanos(
            now.as_nanos().saturating_add(backoff_nanos),
        ));
        self.backoff_attempt = self.backoff_attempt.saturating_add(1);
        self.state = WorkerState::Backoff;
        self.state
    }

    /// Execute the full 7-step constitutional generation-fenced teardown sequence (plan §4).
    pub fn execute_generation_fenced_teardown(&mut self) -> &[TeardownStage] {
        self.teardown_log.clear();

        // 1. Revoke input authority immediately
        self.input_revoked = true;
        self.teardown_log.push(TeardownStage::RevokeInputAuthority);

        // 2. Release remotely held keys/buttons and invalidate generations
        self.authority_fenced = true;
        self.generation = self.generation.wrapping_add(1);
        self.teardown_log.push(TeardownStage::InvalidateGenerations);

        // 3. Stop capture admission
        self.teardown_log.push(TeardownStage::StopCaptureAdmission);

        // 4. Cancel cooperative tasks
        self.teardown_log
            .push(TeardownStage::CancelCooperativeTasks);

        // 5. Drain bounded sends
        self.teardown_log.push(TeardownStage::DrainBoundedSends);

        // 6. Kill stuck foreign worker if necessary
        self.teardown_log.push(TeardownStage::TerminateWorker);

        // 7. Reap worker and publish closure
        self.teardown_log.push(TeardownStage::ReapWorker);
        self.teardown_log.push(TeardownStage::Completed);

        if !self.state.is_disabled() {
            self.state = WorkerState::Idle;
        }
        &self.teardown_log
    }

    /// Process typed capability events from the platform adapter.
    pub fn handle_capability_event(
        &mut self,
        event: &WorkerCapabilityEvent,
        now: Time,
    ) -> Result<WorkerState, LifecycleError> {
        match event {
            WorkerCapabilityEvent::PermissionLoss => {
                // Permission loss immediately revokes input, fences authority, and disables profile.
                self.input_revoked = true;
                self.authority_fenced = true;
                self.generation = self.generation.wrapping_add(1);
                self.state = WorkerState::ProfileDisabled(ProfileDisableReason::PermissionDenied);
                Ok(self.state)
            }
            WorkerCapabilityEvent::ProtectedContentDetected => {
                // Protected content is a typed event, NOT treated as network stall or codec crash!
                self.protected_content_active = true;
                // Worker stays running; frames are replaced with blanking/placeholder by capture pipeline.
                Ok(self.state)
            }
            WorkerCapabilityEvent::GpuDeviceLost => {
                // GPU device lost: fence authority and record GPU failure
                self.input_revoked = true;
                self.authority_fenced = true;
                self.generation = self.generation.wrapping_add(1);
                let new_state = self.record_failure(now, true);
                Ok(new_state)
            }
            WorkerCapabilityEvent::GeometryChanged { .. }
            | WorkerCapabilityEvent::DisplayColorChanged { .. }
            | WorkerCapabilityEvent::DisplayRemoved { .. } => {
                // Display environment change requires generation bump for reconfiguration
                self.generation = self.generation.wrapping_add(1);
                Ok(self.state)
            }
        }
    }
}
