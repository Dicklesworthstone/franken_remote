//! Broker daemon configuration (plan sections 5.2, 19.2).
//!
//! Stores local daemon settings with safe defaults:
//! - Default sharing scope is `OwnUser` (never broadcast to whole tailnet silently).
//! - Default approval mode is `PromptAlways` (user must approve every connection).
//! - Audio playback and microphone default to disabled (explicit opt-in per AGENTS.md §3.3).
//! - Default service port is 8443.

use core::fmt;
use core::time::Duration;
use fr_core::limits::ProtocolLimits;
use std::net::IpAddr;

/// Tailnet sharing scope policy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SharingScope {
    /// Only devices owned by the local Tailscale user may connect (default).
    #[default]
    OwnUser,
    /// Any device on the tailnet within tailnet ACLs may connect.
    Tailnet,
}

impl SharingScope {
    #[must_use]
    pub const fn to_tailnet_scope(self) -> fr_tailnet::Scope {
        match self {
            Self::OwnUser => fr_tailnet::Scope::OwnUser,
            Self::Tailnet => fr_tailnet::Scope::Tailnet,
        }
    }

    #[must_use]
    pub const fn from_tailnet_scope(scope: fr_tailnet::Scope) -> Self {
        match scope {
            fr_tailnet::Scope::OwnUser => Self::OwnUser,
            fr_tailnet::Scope::Tailnet => Self::Tailnet,
        }
    }
}

/// Interactive session approval requirement.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Interactive desktop prompt required for every session (default).
    #[default]
    PromptAlways,
    /// Pre-authorized local approvals, prompt otherwise.
    ExplicitLocal,
    /// Unattended / headless access (must be explicitly enabled by administrator).
    Unattended,
}

/// Desktop display selection policy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DesktopSelection {
    /// Capture the primary desktop display (default).
    #[default]
    Primary,
    /// Capture a specific display index.
    DisplayIndex(u32),
    /// Span all detected displays.
    All,
    /// Headless virtual display buffer.
    Headless,
}

/// Audio direction configuration.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AudioSettings {
    /// Host-to-client audio playback (default: disabled, explicit opt-in).
    pub playback_enabled: bool,
    /// Client-to-host microphone input (default: disabled, explicit opt-in per AGENTS.md §3.3).
    pub microphone_enabled: bool,
}

/// Auxiliary data transfer configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferSettings {
    /// Synchronized clipboard sharing (default: true).
    pub clipboard_enabled: bool,
    /// File transfer capability (default: true).
    pub file_transfer_enabled: bool,
}

impl Default for TransferSettings {
    fn default() -> Self {
        Self {
            clipboard_enabled: true,
            file_transfer_enabled: true,
        }
    }
}

/// Local daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Sharing scope limit (own-user by default).
    pub sharing_scope: SharingScope,
    /// User approval requirement for new sessions.
    pub approval_mode: ApprovalMode,
    /// Service port to bind on tailnet interface (default 8443).
    pub service_port: u16,
    /// Optional specific IP address to bind (None = auto-detect tailnet node address).
    pub bind_ip: Option<IpAddr>,
    /// Audio streaming settings.
    pub audio: AudioSettings,
    /// Clipboard and file transfer settings.
    pub transfers: TransferSettings,
    /// Display to capture and stream.
    pub desktop: DesktopSelection,
    /// Inactive session timeout duration before automatic disconnect.
    pub idle_timeout: Duration,
    /// Maximum concurrent viewer sessions allowed.
    pub max_viewers: usize,
    /// Protocol limits governing allocations and buffers.
    pub limits: ProtocolLimits,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            sharing_scope: SharingScope::OwnUser,
            approval_mode: ApprovalMode::PromptAlways,
            service_port: fr_tailnet::DEFAULT_SERVICE_PORT,
            bind_ip: None,
            audio: AudioSettings::default(),
            transfers: TransferSettings::default(),
            desktop: DesktopSelection::Primary,
            idle_timeout: Duration::from_secs(300),
            max_viewers: 4,
            limits: ProtocolLimits::ABSOLUTE,
        }
    }
}

/// Typed errors in daemon configuration validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// Port number is invalid (e.g. 0).
    InvalidPort(u16),
    /// Maximum viewer count is outside supported range [1, 32].
    InvalidMaxViewers(usize),
    /// Idle timeout is zero or exceeds the 24-hour ceiling.
    InvalidIdleTimeout,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPort(port) => write!(f, "invalid service port {port}"),
            Self::InvalidMaxViewers(count) => {
                write!(f, "invalid max viewers {count} (must be 1..=32)")
            }
            Self::InvalidIdleTimeout => write!(f, "invalid idle timeout (must be 1s..=24h)"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl DaemonConfig {
    /// Validate configuration invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.service_port == 0 {
            return Err(ConfigError::InvalidPort(0));
        }
        if self.max_viewers == 0 || self.max_viewers > 32 {
            return Err(ConfigError::InvalidMaxViewers(self.max_viewers));
        }
        if self.idle_timeout.is_zero() || self.idle_timeout > Duration::from_hours(24) {
            return Err(ConfigError::InvalidIdleTimeout);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_and_secure() {
        let config = DaemonConfig::default();
        assert_eq!(config.sharing_scope, SharingScope::OwnUser);
        assert_eq!(config.approval_mode, ApprovalMode::PromptAlways);
        assert_eq!(config.service_port, 8443);
        assert!(!config.audio.playback_enabled);
        assert!(!config.audio.microphone_enabled);
        assert!(config.transfers.clipboard_enabled);
        assert!(config.transfers.file_transfer_enabled);
        assert_eq!(config.max_viewers, 4);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn invalid_port_zero_is_rejected() {
        let config = DaemonConfig {
            service_port: 0,
            ..Default::default()
        };
        assert_eq!(config.validate(), Err(ConfigError::InvalidPort(0)));
    }

    #[test]
    fn invalid_viewer_counts_are_rejected() {
        let config_zero = DaemonConfig {
            max_viewers: 0,
            ..Default::default()
        };
        assert_eq!(
            config_zero.validate(),
            Err(ConfigError::InvalidMaxViewers(0))
        );

        let config_too_many = DaemonConfig {
            max_viewers: 33,
            ..Default::default()
        };
        assert_eq!(
            config_too_many.validate(),
            Err(ConfigError::InvalidMaxViewers(33))
        );
    }

    #[test]
    fn invalid_idle_timeout_is_rejected() {
        let config_zero = DaemonConfig {
            idle_timeout: Duration::ZERO,
            ..Default::default()
        };
        assert_eq!(config_zero.validate(), Err(ConfigError::InvalidIdleTimeout));

        let config_too_long = DaemonConfig {
            idle_timeout: Duration::from_secs(86_401),
            ..Default::default()
        };
        assert_eq!(
            config_too_long.validate(),
            Err(ConfigError::InvalidIdleTimeout)
        );
    }

    #[test]
    fn scope_conversions_match() {
        assert_eq!(
            SharingScope::OwnUser.to_tailnet_scope(),
            fr_tailnet::Scope::OwnUser
        );
        assert_eq!(
            SharingScope::Tailnet.to_tailnet_scope(),
            fr_tailnet::Scope::Tailnet
        );
        assert_eq!(
            SharingScope::from_tailnet_scope(fr_tailnet::Scope::OwnUser),
            SharingScope::OwnUser
        );
        assert_eq!(
            SharingScope::from_tailnet_scope(fr_tailnet::Scope::Tailnet),
            SharingScope::Tailnet
        );
    }
}
