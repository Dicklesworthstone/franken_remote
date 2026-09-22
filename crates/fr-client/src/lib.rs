#![forbid(unsafe_code)]
//! Shared viewer input engine. Transport authentication, renderer evidence and
//! local lifecycle are supplied by the containing native/browser client. This
//! module opens no listener, invents no grant, and never retries an action.
pub mod audio;
pub mod authority;
pub mod clock;
pub mod cursor;
pub mod input;

pub use cursor::{CursorDamageRect, CursorShapeMetadata, PredictedCursor};

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

/// Tailnet peer discovery, bounded capability probes, saved hosts, and directory policy (Plan §6.4).
pub mod discovery;

/// Robot surface, schema-versioned envelopes, and staged acknowledgements (Plan §§18.1, 18.2).
pub mod robot;

/// Connection quality telemetry and evaluation (Plan §16.1).
pub mod connection_quality;
pub use connection_quality::{
    ConnectionQualityMetrics, QualityTier, QualityWarning, TransportType,
};

/// Desktop client settings and preferences (Plan §§14.1, 16.1).
pub mod settings;
pub use settings::{ClientSettings, ColorRangePreference, DisplayFitMode};

/// Platform permissions and user remediation guidance (Plan §16.1).
pub mod permissions;
pub use permissions::{PermissionCategory, PermissionExplanation, PermissionState};

/// Untrusted host HEVC decode admission controller (Plan §16.1, §17.2).
pub mod decoder_admission;
pub use decoder_admission::{DecodeAdmissionRefusal, DecoderAdmissionController};
