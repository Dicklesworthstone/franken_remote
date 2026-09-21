//! Linux platform host adapter: Wayland portal/`PipeWire`, EIS input, and X11 adapter (plan §10.1).
//!
//! Enforces:
//! 1. Session agent ownership: session agent creates and retains the `RemoteDesktop` portal session
//!    and EIS input connection, delegating ONLY the selected `PipeWire` stream capability to the media worker.
//! 2. Strictly ordered portal sequence with clipboard requested before Start.
//! 3. Returned grants define authorization, not requested flags.
//! 4. Atomic single-use restore token persistence and rotation.
//! 5. `PipeWire` stream resolution with monotonic serial and node ID reuse protection.
//! 6. Distinct coordinate systems (compositor space, stream pixels, crop/scale, EIS region).
//! 7. Cursor metadata gating: metadata emitted only when explicitly granted.
//! 8. EIS-preferred input path under safe `#![forbid(unsafe_code)]`.
//! 9. Explicit X11 adapter surfacing unconfined security model diagnostics.
//! 10. Systemd user service graphical session attachment.
//! 11. Compositor qualification matrix (GNOME, KDE, Hyprland/wlroots).

pub mod coordinates;
pub mod cursor;
pub mod eis;
pub mod portal;
pub mod qualification;
pub mod restore_token;
pub mod stream_resolver;
pub mod systemd_session;
pub mod worker_isolation;
pub mod x11;

pub use coordinates::{
    CompositorPoint, CompositorRect, CoordinateConversionError, CropScaleMapping, EisPoint,
    EisRegion, StreamPixelPoint, StreamResolution, compositor_to_eis, compositor_to_stream_pixel,
    eis_to_compositor, stream_pixel_to_compositor,
};

pub use cursor::{CursorManager, CursorMetadata, CursorMetadataRefusal, CursorMode};

pub use eis::{EisEventPoster, EisInputSink, EisRecordedEvent, RecordingEisPoster};

pub use portal::{DeviceFlags, PortalSequenceError, PortalSession, PortalState, SourceTypes};

pub use qualification::{
    CompositorCapability, CompositorFamily, CompositorQualificationRow, QUALIFIED_ROWS,
    QualificationStatus, evaluate_compositor,
};

pub use restore_token::{
    MAX_RESTORE_TOKEN_LEN, RestoreOutcome, RestoreToken, RestoreTokenError, RestoreTokenManager,
};

pub use stream_resolver::{PipeWireStreamInfo, StreamResolver, StreamVerificationError};

pub use systemd_session::{GraphicalSessionEnvironment, SessionEnvironmentError, SessionType};

pub use worker_isolation::{
    PipeWireStreamCapability, WorkerIsolationBoundary, WorkerIsolationError,
};

pub use x11::{
    PlatformSecurityModel, PlatformSecurityReport, RecordingX11Poster, X11EventPoster,
    X11InputSink, X11RecordedEvent,
};
