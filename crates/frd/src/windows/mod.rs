//! Windows platform host adapter: Desktop Duplication, hybrid GPU, and session-aware service (plan §8.3, §10.3).
//!
//! Enforces:
//! 1. Desktop Duplication capture with prompt frame release and `OwnedGpuSurface` copy.
//! 2. Multi-monitor virtual desktop coordinates with negative bounds and DXGI rotation.
//! 3. Display transitions, mode changes, and GPU fault recovery with typed causes.
//! 4. Hybrid GPU qualification and cross-adapter pairing for laptops with iGPU + dGPU.
//! 5. Session 0 isolation invariant: service never captures Session 0 as the user desktop.
//! 6. Session-bound named pipes (`\\.\pipe\frankenremote-session-{session_id}-{token}`).
//! 7. `SendInput` input sink with UIPI integrity gating and typed refusal for elevated windows.
//! 8. Windows qualification matrix and hardware evidence records.

pub mod coordinates;
pub mod duplication;
pub mod hybrid_gpu;
pub mod qualification;
pub mod send_input;
pub mod session_service;
pub mod transitions;

pub use coordinates::{
    CoordinateError, DxgiRotation, NormalizedSendInputPoint, StreamPixelPoint, StreamResolution,
    VirtualDesktopPoint, VirtualDesktopRect, stream_pixel_to_virtual_desktop,
    virtual_desktop_to_send_input,
};

pub use duplication::{
    CursorShapeType, DesktopDuplicationSession, DesktopFrameInfo, DuplicationError,
    DuplicationState, DxgiFormat, DxgiMoveRect, DxgiRect, OwnedGpuSurface, ToneMappingMethod,
    WindowsCursorInfo, WindowsCursorShape,
};

pub use hybrid_gpu::{
    AdapterDesc, AdapterLuid, AdapterPairing, CrossAdapterStrategy, GpuVendor, HardwareEncoderKind,
    HybridGpuSelector, HybridTopology,
};

pub use qualification::{
    QUALIFIED_WINDOWS_ROWS, QualificationStatus, WindowsQualificationRow, WindowsReleaseFamily,
    classify_build_number,
};

pub use send_input::{
    RecordingSendInputPoster, SendInputPoster, SendInputRecordedEvent, TargetWindowSecurity,
    WHEEL_DELTA, WindowsInputSink, WindowsIntegrityLevel,
};

pub use coordinates::NormalizedSendInputPoint as SendInputPoint;

pub use session_service::{
    SessionBoundPipe, SessionLockState, SessionServiceError, WindowsSessionInfo,
    WindowsSessionKind, WindowsSessionManager, WtsSessionEvent,
};

pub use transitions::{
    DxgiErrorCode, TransitionCause, TransitionCoordinator, TransitionLogEntry,
    TransitionRecoveryState,
};
