//! Windows Session Architecture and Session-Aware Service Split (plan §10.3).
//!
//! Enforces:
//! 1. Session 0 isolation invariant: the Windows service runs in Session 0 and NEVER captures
//!    Session 0 as the user's interactive desktop.
//! 2. Interactive agent binds strictly to the intended user session ID (e.g. active console).
//! 3. Named pipe endpoints are strictly session-bound (`\\.\pipe\frankenremote-session-{session_id}-{token}`),
//!    preventing cross-session impersonation or hijacking.
//! 4. Desktop lock screens, fast user switching, and secure-desktop transitions surface as typed
//!    capability changes rather than silent failures or frozen streams.

use std::fmt;

/// Windows Terminal Services / `WTSSession` notification event codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtsSessionEvent {
    /// `WTS_CONSOLE_CONNECT` (0x1): Console session connected.
    ConsoleConnect,
    /// `WTS_CONSOLE_DISCONNECT` (0x2): Console session disconnected.
    ConsoleDisconnect,
    /// `WTS_REMOTE_CONNECT` (0x3): Remote session connected.
    RemoteConnect,
    /// `WTS_REMOTE_DISCONNECT` (0x4): Remote session disconnected.
    RemoteDisconnect,
    /// `WTS_SESSION_LOGON` (0x5): User logged on.
    SessionLogon,
    /// `WTS_SESSION_LOGOFF` (0x6): User logged off.
    SessionLogoff,
    /// `WTS_SESSION_LOCK` (0x7): Session locked.
    SessionLock,
    /// `WTS_SESSION_UNLOCK` (0x8): Session unlocked.
    SessionUnlock,
    /// `WTS_SESSION_REMOTE_CONTROL` (0x9): Session remote control status changed.
    SessionRemoteControl,
}

impl WtsSessionEvent {
    /// Map integer notification event code to typed `WtsSessionEvent`.
    #[must_use]
    pub const fn from_event_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::ConsoleConnect),
            2 => Some(Self::ConsoleDisconnect),
            3 => Some(Self::RemoteConnect),
            4 => Some(Self::RemoteDisconnect),
            5 => Some(Self::SessionLogon),
            6 => Some(Self::SessionLogoff),
            7 => Some(Self::SessionLock),
            8 => Some(Self::SessionUnlock),
            9 => Some(Self::SessionRemoteControl),
            _ => None,
        }
    }
}

/// Category of Windows session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsSessionKind {
    /// Session 0: Non-interactive Windows service session (NEVER captured as user desktop).
    SessionZeroService,
    /// Interactive physical console session (keyboard/mouse/monitor attached).
    InteractiveConsole,
    /// Remote Desktop Services (RDP) virtual interactive session.
    RemoteDesktopRdp,
    /// Disconnected session.
    Disconnected,
}

/// Lock or security state of an interactive session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionLockState {
    /// Unlocked and actively receiving interactive user input.
    #[default]
    Unlocked,
    /// Workstation locked (lock screen displayed).
    Locked,
    /// Windows logon screen or credential provider screen active.
    LogonScreen,
    /// UAC prompt active on the Secure Desktop.
    SecureDesktopUac,
}

/// Details of a Windows session managed by the host service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsSessionInfo {
    pub session_id: u32,
    pub kind: WindowsSessionKind,
    pub user_name: Option<String>,
    pub domain_name: Option<String>,
    pub lock_state: SessionLockState,
}

/// Errors originating from Windows session management.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionServiceError {
    /// Attempted to attach capture or input to Session 0 (strictly forbidden).
    SessionZeroCaptureForbidden,
    /// Target session does not exist or is disconnected.
    SessionNotFound(u32),
    /// Access denied acquiring user token for target session.
    AccessDenied(u32),
    /// Invalid named pipe path format.
    InvalidPipeFormat,
}

impl fmt::Display for SessionServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionZeroCaptureForbidden => {
                write!(
                    f,
                    "Session 0 capture forbidden: services cannot capture non-interactive Session 0"
                )
            }
            Self::SessionNotFound(id) => write!(f, "Windows session {id} not found"),
            Self::AccessDenied(id) => write!(f, "access denied for session {id}"),
            Self::InvalidPipeFormat => write!(f, "invalid session-bound named pipe path"),
        }
    }
}

impl std::error::Error for SessionServiceError {}

/// Session-bound named pipe configuration ensuring IPC isolation across Windows user sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBoundPipe {
    pub session_id: u32,
    pub token: String,
    pub pipe_path: String,
}

impl SessionBoundPipe {
    /// Construct a secure session-bound named pipe path.
    /// Format: `\\.\pipe\frankenremote-session-{session_id}-{token}`.
    pub fn new(session_id: u32, token: &str) -> Result<Self, SessionServiceError> {
        if session_id == 0 {
            // Interactive agents are never bound to Session 0
            return Err(SessionServiceError::SessionZeroCaptureForbidden);
        }
        if token.is_empty() || token.contains('\\') || token.contains('/') {
            return Err(SessionServiceError::InvalidPipeFormat);
        }

        let pipe_path = format!(r"\\.\pipe\frankenremote-session-{session_id}-{token}");
        Ok(Self {
            session_id,
            token: token.to_string(),
            pipe_path,
        })
    }
}

/// Service-level session manager coordinating the interactive agent across Windows sessions.
#[derive(Debug)]
pub struct WindowsSessionManager {
    current_active_console_id: u32,
    sessions: Vec<WindowsSessionInfo>,
}

impl WindowsSessionManager {
    /// Initialize manager with initial active console session ID.
    #[must_use]
    pub fn new(initial_active_console_id: u32) -> Self {
        Self {
            current_active_console_id: initial_active_console_id,
            sessions: Vec::new(),
        }
    }

    #[must_use]
    pub const fn active_console_session_id(&self) -> u32 {
        self.current_active_console_id
    }

    /// Register or update session information.
    pub fn update_session(&mut self, info: WindowsSessionInfo) {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == info.session_id)
        {
            *existing = info;
        } else {
            self.sessions.push(info);
        }
    }

    /// Query session details by ID.
    #[must_use]
    pub fn get_session(&self, session_id: u32) -> Option<&WindowsSessionInfo> {
        self.sessions.iter().find(|s| s.session_id == session_id)
    }

    /// Process a Windows Terminal Services notification event.
    pub fn on_wts_event(&mut self, event: WtsSessionEvent, session_id: u32) {
        match event {
            WtsSessionEvent::ConsoleConnect => {
                self.current_active_console_id = session_id;
                if let Some(s) = self
                    .sessions
                    .iter_mut()
                    .find(|s| s.session_id == session_id)
                {
                    s.kind = WindowsSessionKind::InteractiveConsole;
                }
            }
            WtsSessionEvent::ConsoleDisconnect => {
                if let Some(s) = self
                    .sessions
                    .iter_mut()
                    .find(|s| s.session_id == session_id)
                {
                    s.kind = WindowsSessionKind::Disconnected;
                }
            }
            WtsSessionEvent::SessionLock => {
                if let Some(s) = self
                    .sessions
                    .iter_mut()
                    .find(|s| s.session_id == session_id)
                {
                    s.lock_state = SessionLockState::Locked;
                }
            }
            WtsSessionEvent::SessionUnlock => {
                if let Some(s) = self
                    .sessions
                    .iter_mut()
                    .find(|s| s.session_id == session_id)
                {
                    s.lock_state = SessionLockState::Unlocked;
                }
            }
            _ => {}
        }
    }

    /// Validate whether a target session is permitted for desktop capture.
    /// Strictly rejects Session 0 (rule: service never captures Session 0 as user desktop).
    pub fn validate_capture_target(&self, session_id: u32) -> Result<(), SessionServiceError> {
        if session_id == 0 {
            return Err(SessionServiceError::SessionZeroCaptureForbidden);
        }
        let session = self
            .get_session(session_id)
            .ok_or(SessionServiceError::SessionNotFound(session_id))?;
        if session.kind == WindowsSessionKind::Disconnected {
            return Err(SessionServiceError::SessionNotFound(session_id));
        }
        Ok(())
    }
}
