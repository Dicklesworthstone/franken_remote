//! Display transitions, desktop switches, and GPU fault recovery for Windows (plan §10.3).
//!
//! Enforces:
//! 1. Explicit handling of `DXGI_ERROR_ACCESS_LOST`, `DXGI_ERROR_DEVICE_REMOVED`, `DXGI_ERROR_DEVICE_RESET`,
//!    and `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`.
//! 2. Clean separation between display/GPU transitions and transport network failures.
//! 3. Structured transition event logging with typed causes (lock, unlock, mode change, GPU reset).
//! 4. Recovery state machine with bounded retry and exponential backoff.

use std::fmt;

use super::coordinates::{DxgiRotation, StreamResolution};

/// Native DXGI HRESULT status codes mapped to typed transition errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DxgiErrorCode {
    /// 0x887A0026: Desktop duplication access lost (desktop switch, lock, UAC prompt).
    AccessLost,
    /// 0x887A0005: Hardware device removed or crashed (TDR reset).
    DeviceRemoved,
    /// 0x887A0007: Device reset due to hardware reconfiguration.
    DeviceReset,
    /// 0x887A0027: Terminal services / remote session disconnected.
    SessionDisconnected,
    /// 0x887A0022: Maximum concurrent duplication sessions exhausted (limit typically 4).
    NotCurrentlyAvailable,
    /// 0x887A0004: Unsupported output or adapter configuration.
    Unsupported,
    /// Unrecognized DXGI error code.
    Other(i32),
}

impl DxgiErrorCode {
    /// Map a 32-bit Windows HRESULT to typed `DxgiErrorCode`.
    #[must_use]
    pub const fn from_hresult(hr: i32) -> Self {
        match hr.cast_unsigned() {
            0x887A_0026 => Self::AccessLost,
            0x887A_0005 => Self::DeviceRemoved,
            0x887A_0007 => Self::DeviceReset,
            0x887A_0027 => Self::SessionDisconnected,
            0x887A_0022 => Self::NotCurrentlyAvailable,
            0x887A_0004 => Self::Unsupported,
            _ => Self::Other(hr),
        }
    }

    /// Convert back to standard 32-bit HRESULT.
    #[must_use]
    pub const fn to_hresult(self) -> i32 {
        #[allow(clippy::cast_possible_wrap)]
        match self {
            Self::AccessLost => 0x887A_0026_u32 as i32,
            Self::DeviceRemoved => 0x887A_0005_u32 as i32,
            Self::DeviceReset => 0x887A_0007_u32 as i32,
            Self::SessionDisconnected => 0x887A_0027_u32 as i32,
            Self::NotCurrentlyAvailable => 0x887A_0022_u32 as i32,
            Self::Unsupported => 0x887A_0004_u32 as i32,
            Self::Other(hr) => hr,
        }
    }
}

/// Typed cause of a desktop or display transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionCause {
    /// Workstation locked or lock screen displayed.
    LockScreen,
    /// Workstation unlocked by authorized user.
    Unlock,
    /// User Account Control (UAC) secure desktop prompt invoked.
    SecureDesktopUac,
    /// Fast User Switching to a different user session.
    UserSwitch,
    /// Display resolution or mode changed.
    ResolutionChange {
        old_res: StreamResolution,
        new_res: StreamResolution,
    },
    /// Display orientation rotation changed.
    RotationChange {
        old_rotation: DxgiRotation,
        new_rotation: DxgiRotation,
    },
    /// GPU TDR (Timeout Detection and Recovery) or driver reset occurred.
    GpuDeviceReset { device_removed_hr: i32 },
    /// Duplication sessions exhausted by concurrent applications.
    DuplicationExhaustion,
}

impl fmt::Display for TransitionCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LockScreen => write!(f, "workstation locked (session locked)"),
            Self::Unlock => write!(f, "workstation unlocked"),
            Self::SecureDesktopUac => write!(f, "UAC secure desktop prompt active"),
            Self::UserSwitch => write!(f, "switched to another user session"),
            Self::ResolutionChange { old_res, new_res } => {
                write!(
                    f,
                    "display resolution changed from {old_res:?} to {new_res:?}"
                )
            }
            Self::RotationChange {
                old_rotation,
                new_rotation,
            } => {
                write!(
                    f,
                    "display rotation changed from {old_rotation:?} to {new_rotation:?}"
                )
            }
            Self::GpuDeviceReset { device_removed_hr } => {
                write!(
                    f,
                    "GPU device reset / TDR occurred (HRESULT: 0x{device_removed_hr:08X})"
                )
            }
            Self::DuplicationExhaustion => {
                write!(
                    f,
                    "Desktop Duplication session limit reached on display adapter"
                )
            }
        }
    }
}

/// Logged record of a display or desktop transition for auditing and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionLogEntry {
    pub timestamp_millis: u64,
    pub cause: TransitionCause,
    pub retry_count: u32,
    pub recovery_successful: bool,
}

/// State of the transition recovery state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransitionRecoveryState {
    /// Normal operation, capture is healthy.
    #[default]
    Normal,
    /// Transition detected, waiting for retry backoff or desktop availability.
    AwaitingRetry {
        attempt: u32,
        next_retry_millis: u64,
    },
    /// Re-enumerating DXGI outputs and recreating duplication interface.
    RecreatingDuplication,
    /// Unrecoverable fault (e.g. fatal device removal without hardware recovery).
    FatalFault,
}

/// Transition and recovery coordinator.
#[derive(Debug, Default)]
pub struct TransitionCoordinator {
    state: TransitionRecoveryState,
    history: Vec<TransitionLogEntry>,
    max_retries: u32,
    base_backoff_millis: u64,
}

impl TransitionCoordinator {
    /// Create a new transition coordinator with default retry parameters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: TransitionRecoveryState::Normal,
            history: Vec::new(),
            max_retries: 10,
            base_backoff_millis: 100,
        }
    }

    #[must_use]
    pub const fn state(&self) -> TransitionRecoveryState {
        self.state
    }

    #[must_use]
    pub fn history(&self) -> &[TransitionLogEntry] {
        &self.history
    }

    /// Handle a detected DXGI error code, transition the state machine, and log the event.
    pub fn on_dxgi_fault(
        &mut self,
        code: DxgiErrorCode,
        timestamp_millis: u64,
    ) -> TransitionRecoveryState {
        let cause = match code {
            DxgiErrorCode::DeviceRemoved | DxgiErrorCode::DeviceReset => {
                TransitionCause::GpuDeviceReset {
                    device_removed_hr: code.to_hresult(),
                }
            }
            DxgiErrorCode::NotCurrentlyAvailable => TransitionCause::DuplicationExhaustion,
            DxgiErrorCode::SessionDisconnected => TransitionCause::UserSwitch,
            DxgiErrorCode::AccessLost | DxgiErrorCode::Unsupported | DxgiErrorCode::Other(_) => {
                TransitionCause::LockScreen
            }
        };

        let current_attempt = match self.state {
            TransitionRecoveryState::AwaitingRetry { attempt, .. } => attempt.saturating_add(1),
            _ => 1,
        };

        if current_attempt > self.max_retries {
            self.state = TransitionRecoveryState::FatalFault;
            self.history.push(TransitionLogEntry {
                timestamp_millis,
                cause,
                retry_count: current_attempt,
                recovery_successful: false,
            });
            return self.state;
        }

        let backoff = self
            .base_backoff_millis
            .saturating_mul(1u64 << current_attempt.min(6));
        let next_retry_millis = timestamp_millis.saturating_add(backoff);

        self.state = TransitionRecoveryState::AwaitingRetry {
            attempt: current_attempt,
            next_retry_millis,
        };

        self.history.push(TransitionLogEntry {
            timestamp_millis,
            cause,
            retry_count: current_attempt,
            recovery_successful: false,
        });

        self.state
    }

    /// Signal that a transition recovery succeeded.
    pub fn on_recovery_succeeded(&mut self, timestamp_millis: u64, cause: TransitionCause) {
        let attempts = match self.state {
            TransitionRecoveryState::AwaitingRetry { attempt, .. } => attempt,
            _ => 0,
        };
        self.state = TransitionRecoveryState::Normal;
        self.history.push(TransitionLogEntry {
            timestamp_millis,
            cause,
            retry_count: attempts,
            recovery_successful: true,
        });
    }
}
