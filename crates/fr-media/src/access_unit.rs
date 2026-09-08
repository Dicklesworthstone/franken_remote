//! Encoded access units and their identity/dependency metadata (plan sections
//! 8.1, 12.3).
//!
//! An [`EncodedAccessUnit`] is one complete admitted picture: bytes plus the
//! metadata that fences it against stale configuration and lets the receiver
//! reason about reference dependencies. The bytes stay opaque; this crate does
//! not parse the HEVC bitstream (that bounded validation lives at the wire
//! boundary and in the decoder-configuration path). Length is validated
//! against the shared limits at construction so an over-ceiling unit can never
//! be built.

use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_core::limits::{LimitsError, ProtocolLimits};

/// A monotonically increasing per-stream frame identity. Distinct from the
/// generations: it names *which* picture, not which configuration epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FrameId(u64);

impl FrameId {
    /// The first frame of a stream.
    pub const FIRST: Self = Self(0);

    /// Wraps a raw frame number.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw frame number.
    #[must_use]
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// The next frame id, or `None` at exhaustion (which forces a stream
    /// restart rather than reuse).
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }
}

/// The reference role of an access unit in the low-delay baseline chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// An independently decodable IDR: a clean reference-chain reset used on
    /// startup, reconfiguration, and recovery. Carries the recovery generation
    /// it establishes.
    Idr {
        /// The recovery generation this IDR begins.
        recovery: RecoveryGeneration,
    },
    /// A predicted picture depending on exactly one previous reference frame
    /// in the baseline chain.
    Predicted {
        /// The frame this picture references.
        references: FrameId,
    },
}

impl FrameKind {
    /// True for an IDR (a reference-chain reset).
    #[must_use]
    pub const fn is_idr(self) -> bool {
        matches!(self, Self::Idr { .. })
    }
}

/// One complete encoded access unit. Construction validates the byte length
/// against the negotiated limits, so an over-ceiling unit is a typed refusal
/// and can never exist as a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAccessUnit {
    frame: FrameId,
    kind: FrameKind,
    config_generation: CodecConfigurationGeneration,
    /// Host-monotonic capture time in microseconds (presentation timestamp).
    capture_micros: u64,
    bytes: Vec<u8>,
}

impl EncodedAccessUnit {
    /// Builds an access unit, validating its length against the negotiated
    /// access-unit ceiling. An IDR must be non-empty; a zero-length picture is
    /// never valid.
    pub fn new(
        limits: &ProtocolLimits,
        frame: FrameId,
        kind: FrameKind,
        config_generation: CodecConfigurationGeneration,
        capture_micros: u64,
        bytes: Vec<u8>,
    ) -> Result<Self, LimitsError> {
        limits.validate_access_unit_len(bytes.len())?;
        if bytes.is_empty() {
            return Err(LimitsError::ZeroDimension);
        }
        Ok(Self {
            frame,
            kind,
            config_generation,
            capture_micros,
            bytes,
        })
    }

    /// The frame identity.
    #[must_use]
    pub const fn frame(&self) -> FrameId {
        self.frame
    }
    /// The reference kind.
    #[must_use]
    pub const fn kind(&self) -> FrameKind {
        self.kind
    }
    /// The configuration generation this unit was encoded under.
    #[must_use]
    pub const fn config_generation(&self) -> CodecConfigurationGeneration {
        self.config_generation
    }
    /// Host-monotonic capture time in microseconds.
    #[must_use]
    pub const fn capture_micros(&self) -> u64 {
        self.capture_micros
    }
    /// The opaque encoded bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Convenience: whether this is an IDR.
    #[must_use]
    pub const fn is_idr(&self) -> bool {
        self.kind.is_idr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen0() -> CodecConfigurationGeneration {
        CodecConfigurationGeneration::INITIAL
    }

    #[test]
    fn frame_id_advances_and_refuses_to_wrap() {
        assert_eq!(FrameId::FIRST.as_raw(), 0);
        assert_eq!(FrameId::FIRST.next(), Some(FrameId::from_raw(1)));
        assert_eq!(FrameId::from_raw(u64::MAX).next(), None);
    }

    #[test]
    fn access_unit_validates_length_and_nonempty() {
        let l = ProtocolLimits::ABSOLUTE;
        let idr = FrameKind::Idr {
            recovery: RecoveryGeneration::INITIAL,
        };
        let ok = EncodedAccessUnit::new(&l, FrameId::FIRST, idr, gen0(), 1_000, vec![1, 2, 3]);
        assert!(ok.is_ok());
        assert!(ok.unwrap().is_idr());

        // Empty is refused.
        assert_eq!(
            EncodedAccessUnit::new(&l, FrameId::FIRST, idr, gen0(), 0, vec![]),
            Err(LimitsError::ZeroDimension)
        );

        // Over-ceiling is refused without allocating the picture as valid.
        let too_big = vec![0u8; (l.max_encoded_access_unit_bytes() as usize) + 1];
        assert!(matches!(
            EncodedAccessUnit::new(&l, FrameId::FIRST, idr, gen0(), 0, too_big),
            Err(LimitsError::AboveCeiling { .. })
        ));
    }

    #[test]
    fn predicted_frames_carry_their_reference() {
        let l = ProtocolLimits::ABSOLUTE;
        let kind = FrameKind::Predicted {
            references: FrameId::FIRST,
        };
        let au = EncodedAccessUnit::new(&l, FrameId::from_raw(1), kind, gen0(), 16_000, vec![9])
            .unwrap();
        assert!(!au.is_idr());
        match au.kind() {
            FrameKind::Predicted { references } => assert_eq!(references, FrameId::FIRST),
            FrameKind::Idr { .. } => panic!("expected predicted"),
        }
    }
}
