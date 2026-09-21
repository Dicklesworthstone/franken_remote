#![forbid(unsafe_code)]
//! Shared viewer input engine. Transport authentication, renderer evidence and
//! local lifecycle are supplied by the containing native/browser client. This
//! module opens no listener, invents no grant, and never retries an action.
pub mod audio;
pub mod authority;
pub mod clock;
pub mod input;

/// Shared native session startup, independent of the windowing/transport adapter.
pub mod startup;

/// One-use initial input-grant negotiation.
pub mod control_grant;

/// Controller-only clipboard tied to the original accepted grant.
pub mod clipboard;

/// Shared client session core, auto-reconnect, presentation policy, and input fencing.
pub mod session;
pub use session::{
    ClientSession, CloseReason, DecoderScheduler, QueuedPicture, ReconnectPolicy, ReconnectReason,
    SessionDiagnostics, SessionError, SessionState, SuspendReason,
};

/// Implemented native observation capability profile, not transport qualification.
pub mod native;

/// Per-platform system shortcut capture and routing capability table (Plan §§15.1, 16.1).
pub mod shortcut;
pub use shortcut::{
    PlatformCapabilityRow, PlatformId, ShortcutCaptureController, ShortcutCaptureError,
    ShortcutCaptureMode, ShortcutRoutingMechanism, ShortcutSupportStatus,
};

/// Minimal desktop client toolbar model (Plan §16.1).
pub mod toolbar;
pub use toolbar::ToolbarModel;
