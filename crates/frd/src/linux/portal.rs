//! Wayland `RemoteDesktop` & `ScreenCast` portal session state machine (plan §10.1).
//!
//! Enforces:
//! 1. Strictly ordered portal sequence:
//!    `CreateSession` -> `SelectDevices` -> `SelectSources` -> `ConfigureClipboard` -> `Start`.
//! 2. Clipboard requested BEFORE `Start`: configuring clipboard after `Start` is rejected.
//! 3. Returned grants, NOT requested flags, define authorization:
//!    - If keyboard was requested but denied by user/compositor, input injection refuses key events.
//!    - If clipboard was requested but denied, clipboard integration is disabled.
//! 4. Agent ownership: the session agent creates and retains the portal session;
//!    calling `into_worker_capability` extracts only the scoped `PipeWire` stream capability.

use core::fmt;

use super::cursor::CursorMode;
use super::stream_resolver::PipeWireStreamInfo;
use super::worker_isolation::PipeWireStreamCapability;

/// Bitflags representing input device categories requested or granted by the portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceFlags(u32);

impl DeviceFlags {
    pub const NONE: Self = Self(0);
    pub const POINTER: Self = Self(1 << 0);
    pub const KEYBOARD: Self = Self(1 << 1);
    pub const TOUCHSCREEN: Self = Self(1 << 2);

    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn as_raw(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub const fn is_pointer(self) -> bool {
        self.contains(Self::POINTER)
    }

    #[must_use]
    pub const fn is_keyboard(self) -> bool {
        self.contains(Self::KEYBOARD)
    }

    #[must_use]
    pub const fn is_touchscreen(self) -> bool {
        self.contains(Self::TOUCHSCREEN)
    }
}

/// Source capture types for `SelectSources`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTypes(u32);

impl SourceTypes {
    pub const MONITOR: Self = Self(1 << 0);
    pub const WINDOW: Self = Self(1 << 1);

    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn as_raw(self) -> u32 {
        self.0
    }
}

/// State of the Wayland portal session state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortalState {
    /// Initial uninitialized state.
    Initial,
    /// Session created via `org.freedesktop.portal.RemoteDesktop.CreateSession`.
    SessionCreated { session_handle: String },
    /// Devices selected via `SelectDevices`.
    DevicesSelected {
        session_handle: String,
        requested_devices: DeviceFlags,
    },
    /// Sources selected via `SelectSources`.
    SourcesSelected {
        session_handle: String,
        requested_devices: DeviceFlags,
        requested_cursor_mode: CursorMode,
        presented_restore_token: Option<String>,
    },
    /// Clipboard integration configured (must occur before `Start`).
    ClipboardConfigured {
        session_handle: String,
        requested_devices: DeviceFlags,
        requested_cursor_mode: CursorMode,
        clipboard_requested: bool,
        presented_restore_token: Option<String>,
    },
    /// Portal session started: grants confirmed, streams established.
    Started {
        session_handle: String,
        granted_devices: DeviceFlags,
        granted_cursor_mode: CursorMode,
        clipboard_granted: bool,
        streams: Vec<PipeWireStreamInfo>,
        replacement_restore_token: Option<String>,
    },
    /// Portal session closed or revoked.
    Closed,
}

/// Typed error when portal sequence invariants or grants are violated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortalSequenceError {
    InvalidStateTransition {
        expected: &'static str,
        actual: &'static str,
    },
    ClipboardConfiguredAfterStart,
    SessionClosed,
    DeviceNotGranted {
        device: &'static str,
    },
    NoStreamsGranted,
    CompositorRefusal(String),
}

impl fmt::Display for PortalSequenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStateTransition { expected, actual } => {
                write!(
                    f,
                    "invalid portal state transition: expected {expected}, actual is {actual}"
                )
            }
            Self::ClipboardConfiguredAfterStart => {
                write!(
                    f,
                    "clipboard integration must be requested before Start, never after"
                )
            }
            Self::SessionClosed => write!(f, "portal session is closed"),
            Self::DeviceNotGranted { device } => {
                write!(f, "requested device '{device}' was not granted by portal")
            }
            Self::NoStreamsGranted => write!(f, "no media streams were granted by compositor"),
            Self::CompositorRefusal(msg) => write!(f, "compositor portal refusal: {msg}"),
        }
    }
}

impl std::error::Error for PortalSequenceError {}

/// Agent-owned Wayland `RemoteDesktop` and `ScreenCast` portal session.
pub struct PortalSession {
    state: PortalState,
    session_id: String,
}

impl PortalSession {
    /// Create a new session tracking structure for a session ID.
    #[must_use]
    pub fn new(session_id: &str) -> Self {
        Self {
            state: PortalState::Initial,
            session_id: session_id.to_string(),
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub const fn state(&self) -> &PortalState {
        &self.state
    }

    #[must_use]
    pub fn is_started(&self) -> bool {
        matches!(self.state, PortalState::Started { .. })
    }

    /// Step 1: Record `CreateSession` response with the returned D-Bus session handle.
    pub fn on_session_created(&mut self, session_handle: &str) -> Result<(), PortalSequenceError> {
        match &self.state {
            PortalState::Initial => {
                self.state = PortalState::SessionCreated {
                    session_handle: session_handle.to_string(),
                };
                Ok(())
            }
            _ => Err(PortalSequenceError::InvalidStateTransition {
                expected: "Initial",
                actual: self.state_name(),
            }),
        }
    }

    /// Step 2: Record `SelectDevices` request.
    pub fn on_select_devices(
        &mut self,
        requested_devices: DeviceFlags,
    ) -> Result<(), PortalSequenceError> {
        match &self.state {
            PortalState::SessionCreated { session_handle } => {
                self.state = PortalState::DevicesSelected {
                    session_handle: session_handle.clone(),
                    requested_devices,
                };
                Ok(())
            }
            _ => Err(PortalSequenceError::InvalidStateTransition {
                expected: "SessionCreated",
                actual: self.state_name(),
            }),
        }
    }

    /// Step 3: Record `SelectSources` request.
    pub fn on_select_sources(
        &mut self,
        requested_cursor_mode: CursorMode,
        presented_restore_token: Option<String>,
    ) -> Result<(), PortalSequenceError> {
        match &self.state {
            PortalState::DevicesSelected {
                session_handle,
                requested_devices,
            } => {
                self.state = PortalState::SourcesSelected {
                    session_handle: session_handle.clone(),
                    requested_devices: *requested_devices,
                    requested_cursor_mode,
                    presented_restore_token,
                };
                Ok(())
            }
            _ => Err(PortalSequenceError::InvalidStateTransition {
                expected: "DevicesSelected",
                actual: self.state_name(),
            }),
        }
    }

    /// Step 4: Configure optional clipboard integration BEFORE `Start`.
    pub fn on_configure_clipboard(
        &mut self,
        clipboard_requested: bool,
    ) -> Result<(), PortalSequenceError> {
        match &self.state {
            PortalState::SourcesSelected {
                session_handle,
                requested_devices,
                requested_cursor_mode,
                presented_restore_token,
            } => {
                self.state = PortalState::ClipboardConfigured {
                    session_handle: session_handle.clone(),
                    requested_devices: *requested_devices,
                    requested_cursor_mode: *requested_cursor_mode,
                    clipboard_requested,
                    presented_restore_token: presented_restore_token.clone(),
                };
                Ok(())
            }
            PortalState::Started { .. } => Err(PortalSequenceError::ClipboardConfiguredAfterStart),
            _ => Err(PortalSequenceError::InvalidStateTransition {
                expected: "SourcesSelected",
                actual: self.state_name(),
            }),
        }
    }

    /// Step 5: Process portal `Start` response.
    ///
    /// The authorization is strictly defined by returned grants, NOT requested flags.
    pub fn on_started(
        &mut self,
        granted_devices: DeviceFlags,
        granted_cursor_mode: CursorMode,
        clipboard_granted: bool,
        streams: Vec<PipeWireStreamInfo>,
        replacement_restore_token: Option<String>,
    ) -> Result<(), PortalSequenceError> {
        if streams.is_empty() {
            return Err(PortalSequenceError::NoStreamsGranted);
        }

        match &self.state {
            PortalState::ClipboardConfigured { session_handle, .. } => {
                self.state = PortalState::Started {
                    session_handle: session_handle.clone(),
                    granted_devices,
                    granted_cursor_mode,
                    clipboard_granted,
                    streams,
                    replacement_restore_token,
                };
                Ok(())
            }
            _ => Err(PortalSequenceError::InvalidStateTransition {
                expected: "ClipboardConfigured",
                actual: self.state_name(),
            }),
        }
    }

    /// Check if pointer injection is granted.
    #[must_use]
    pub fn is_pointer_granted(&self) -> bool {
        match &self.state {
            PortalState::Started {
                granted_devices, ..
            } => granted_devices.is_pointer(),
            _ => false,
        }
    }

    /// Check if keyboard injection is granted.
    #[must_use]
    pub fn is_keyboard_granted(&self) -> bool {
        match &self.state {
            PortalState::Started {
                granted_devices, ..
            } => granted_devices.is_keyboard(),
            _ => false,
        }
    }

    /// Check if clipboard is granted.
    #[must_use]
    pub fn is_clipboard_granted(&self) -> bool {
        match &self.state {
            PortalState::Started {
                clipboard_granted, ..
            } => *clipboard_granted,
            _ => false,
        }
    }

    /// Retrieve replacement restore token if offered by compositor.
    #[must_use]
    pub fn replacement_restore_token(&self) -> Option<&str> {
        match &self.state {
            PortalState::Started {
                replacement_restore_token,
                ..
            } => replacement_restore_token.as_deref(),
            _ => None,
        }
    }

    /// Extract worker stream capability for the media worker.
    ///
    /// CRITICAL INVARIANT: The returned capability contains ONLY the `PipeWire` stream info;
    /// the portal session handle and input connection are retained exclusively by this agent.
    pub fn extract_worker_capability(
        &self,
        stream_index: usize,
        pipewire_remote_fd: Option<i32>,
    ) -> Result<PipeWireStreamCapability, PortalSequenceError> {
        match &self.state {
            PortalState::Started { streams, .. } => {
                let stream = streams
                    .get(stream_index)
                    .ok_or(PortalSequenceError::NoStreamsGranted)?;
                Ok(PipeWireStreamCapability::from_stream_info(
                    stream,
                    pipewire_remote_fd,
                ))
            }
            _ => Err(PortalSequenceError::InvalidStateTransition {
                expected: "Started",
                actual: self.state_name(),
            }),
        }
    }

    /// Close the portal session.
    pub fn close(&mut self) {
        self.state = PortalState::Closed;
    }

    fn state_name(&self) -> &'static str {
        match &self.state {
            PortalState::Initial => "Initial",
            PortalState::SessionCreated { .. } => "SessionCreated",
            PortalState::DevicesSelected { .. } => "DevicesSelected",
            PortalState::SourcesSelected { .. } => "SourcesSelected",
            PortalState::ClipboardConfigured { .. } => "ClipboardConfigured",
            PortalState::Started { .. } => "Started",
            PortalState::Closed => "Closed",
        }
    }
}
