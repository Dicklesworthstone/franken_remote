//! Robot command surface and schema-versioned JSON envelope.
//!
//! Per plan sections 18.1 and 18.2:
//! - Commands:
//!   - `fr hosts [--json]`
//!   - `fr status [--json]`
//!   - `fr inspect <host> [--json]`
//!   - `fr connect <host> [--display N] [--view-only]`
//!   - `fr disconnect <host> [--json]`
//!   - `fr robot session open <host> [--role view|control] [--json]`
//!   - `fr robot observe <host> [--display N] [--json]`
//!   - `fr robot input <host> --lease LEASE --request-id ID ... [--json]`
//!   - `fr robot session close <host> [--json]`
//! - Schema-versioned envelope (version 1) with timestamps, outcome, and typed error codes.
//! - Acknowledgement stages: `admitted`, `submitted_to_os`, `observed` (never "exactly once").
//! - Outcomes: `success`, `partial_submission`, `cancellation`, `refusal`, `unknown_external_effect`.
//! - Commands reuse live local client session; handles are opaque and bearer material
//!   is excluded from argv and logs.

pub mod envelope;
pub mod input;
pub mod inspect;
pub mod observe;
pub mod session;
pub mod sha256;
pub mod status;

pub use envelope::{
    AcknowledgementStage, ROBOT_SCHEMA_VERSION, RobotEnvelope, RobotError, RobotOutcome,
};
pub use input::{
    InputDisposition, ObservedApplicationResult, RobotInputAction, RobotInputData,
    RobotInputRequest, SemanticEvidenceType, WindowFocusPrecondition,
};
pub use inspect::RobotInspectData;
pub use observe::{DisplayGeometryInfo, EvidenceLevel, ObservationArtifact, RobotObservationData};
pub use session::{
    RobotSessionCloseData, RobotSessionLimits, RobotSessionOpenData, RobotSessionRole,
};
pub use status::RobotStatusData;
