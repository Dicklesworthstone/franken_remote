//! `PipeWire` stream resolution and node-ID-reuse protection (plan §10.1).
//!
//! Numeric `PipeWire` node IDs are 32-bit integers recycled by the server when
//! nodes are destroyed. A newly created stream may receive the exact same node ID
//! as a previously closed stream.
//!
//! To prevent attaching to an unrelated `PipeWire` node:
//! 1. Streams are resolved by the strongest identifier available:
//!    - Prefer `pipewire.serial` (monotonic 64-bit integer across daemon lifetime).
//!    - Fallback to unique portal session token + node identifier.
//! 2. On reconnect or renegotiation, stream consistency is strictly re-verified:
//!    - Matching node ID with differing serial is typed-refused as `NodeIdReused`.
//!    - Unexpected resolution changes without explicit reconfiguration are rejected.

use super::coordinates::StreamResolution;
use core::fmt;
use std::collections::HashMap;

/// Identified `PipeWire` screen cast stream from portal response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireStreamInfo {
    /// `PipeWire` node ID (reusable 32-bit integer).
    pub node_id: u32,
    /// Monotonic 64-bit `PipeWire` serial (`pipewire.serial`), if provided by compositor.
    pub serial: Option<u64>,
    /// Strongest unique identifier (serial string or composite session+node).
    pub unique_identifier: String,
    /// Stream pixel resolution.
    pub resolution: StreamResolution,
    /// Stream metadata properties returned by portal.
    pub properties: HashMap<String, String>,
}

impl PipeWireStreamInfo {
    /// Construct a new stream descriptor, automatically deriving the strongest identifier.
    pub fn new(
        node_id: u32,
        serial: Option<u64>,
        session_token: &str,
        resolution: StreamResolution,
        properties: HashMap<String, String>,
    ) -> Self {
        let unique_identifier = match serial {
            Some(s) => format!("serial:{s}"),
            None => format!("session:{session_token}:node:{node_id}"),
        };

        Self {
            node_id,
            serial,
            unique_identifier,
            resolution,
            properties,
        }
    }
}

/// Typed error when re-verifying an active stream after reconnect or renegotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamVerificationError {
    /// Node ID was recycled by the server; the active node is NOT the original stream.
    NodeIdReused {
        node_id: u32,
        expected_serial: Option<u64>,
        actual_serial: Option<u64>,
    },
    /// Monotonic serial does not match expected value.
    SerialMismatch { expected: u64, actual: u64 },
    /// Expected stream identifier was not found in active streams.
    StreamNotFound { identifier: String },
    /// Stream resolution changed unexpectedly across reconnect.
    ResolutionChanged {
        expected: StreamResolution,
        actual: StreamResolution,
    },
}

impl fmt::Display for StreamVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeIdReused {
                node_id,
                expected_serial,
                actual_serial,
            } => {
                write!(
                    f,
                    "PipeWire node ID {node_id} was reused by a different stream (expected serial {expected_serial:?}, found {actual_serial:?})"
                )
            }
            Self::SerialMismatch { expected, actual } => {
                write!(
                    f,
                    "PipeWire serial mismatch across reconnect: expected {expected}, found {actual}"
                )
            }
            Self::StreamNotFound { identifier } => {
                write!(f, "PipeWire stream '{identifier}' not found")
            }
            Self::ResolutionChanged { expected, actual } => {
                write!(
                    f,
                    "PipeWire stream resolution changed unexpectedly from {expected:?} to {actual:?}"
                )
            }
        }
    }
}

impl std::error::Error for StreamVerificationError {}

/// Resolver tracking active stream identities and enforcing node-ID-reuse protection.
#[derive(Default)]
pub struct StreamResolver {
    registered_streams: HashMap<String, PipeWireStreamInfo>,
}

impl StreamResolver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an initial stream after portal Start.
    pub fn register_stream(&mut self, stream: PipeWireStreamInfo) {
        self.registered_streams
            .insert(stream.unique_identifier.clone(), stream);
    }

    /// Get registered stream by unique identifier.
    #[must_use]
    pub fn get_stream(&self, identifier: &str) -> Option<&PipeWireStreamInfo> {
        self.registered_streams.get(identifier)
    }

    /// Re-verify a stream's identity and properties after reconnect.
    pub fn verify_stream(
        &self,
        expected_identifier: &str,
        current: &PipeWireStreamInfo,
    ) -> Result<(), StreamVerificationError> {
        let expected = self
            .registered_streams
            .get(expected_identifier)
            .ok_or_else(|| StreamVerificationError::StreamNotFound {
                identifier: expected_identifier.to_string(),
            })?;

        // Check 1: Monotonic serial match if available
        if let (Some(exp_serial), Some(cur_serial)) = (expected.serial, current.serial) {
            if exp_serial != cur_serial {
                if expected.node_id == current.node_id {
                    return Err(StreamVerificationError::NodeIdReused {
                        node_id: expected.node_id,
                        expected_serial: Some(exp_serial),
                        actual_serial: Some(cur_serial),
                    });
                }
                return Err(StreamVerificationError::SerialMismatch {
                    expected: exp_serial,
                    actual: cur_serial,
                });
            }
        } else if expected.node_id == current.node_id
            && expected.unique_identifier != current.unique_identifier
        {
            // Node IDs match but composite identifiers mismatch -> node ID reuse
            return Err(StreamVerificationError::NodeIdReused {
                node_id: expected.node_id,
                expected_serial: expected.serial,
                actual_serial: current.serial,
            });
        }

        // Check 2: Resolution stability
        if expected.resolution != current.resolution {
            return Err(StreamVerificationError::ResolutionChanged {
                expected: expected.resolution,
                actual: current.resolution,
            });
        }

        Ok(())
    }
}
