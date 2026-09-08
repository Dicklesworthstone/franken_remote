//! Encoded access units and their identity/dependency metadata (plan sections
//! 8.1, 12.3).
//!
//! An [`EncodedAccessUnit`] holds length-validated bytes and declared metadata.
//! This crate does not parse HEVC or authenticate metadata: bounded bitstream
//! validation, reference-chain checks, and session admission remain mandatory
//! before decoder submission. Construction checks length, not those stronger
//! properties. Diagnostic formatting deliberately never exposes screen bytes.

use core::fmt;

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

/// The declared reference role in the low-delay baseline chain. Bitstream
/// validation must establish that the encoded picture actually has this role.
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
    /// True for a declared IDR (independent validation still required).
    #[must_use]
    pub const fn is_idr(self) -> bool {
        matches!(self, Self::Idr { .. })
    }
}

/// One nonempty, length-validated access unit. The constructor does not
/// establish that these opaque bytes encode a conforming HEVC picture.
/// `Debug` includes bounded metadata and byte length, never compressed pixels.
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedAccessUnit {
    frame: FrameId,
    kind: FrameKind,
    config_generation: CodecConfigurationGeneration,
    /// Host-monotonic capture time in microseconds (presentation timestamp).
    capture_micros: u64,
    bytes: Vec<u8>,
}

impl fmt::Debug for EncodedAccessUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedAccessUnit")
            .field("frame", &self.frame)
            .field("kind", &self.kind)
            .field("config_generation", &self.config_generation)
            .field("capture_micros", &self.capture_micros)
            .field("byte_len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

impl EncodedAccessUnit {
    /// Builds an access unit, validating its length against the negotiated
    /// access-unit ceiling. A zero-length picture is never valid. Callers
    /// must admit allocation budgets before assembling the supplied vector.
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
    /// The declared reference kind.
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
    /// The opaque encoded bytes. These contain screen content and must not
    /// be included in ordinary logs or diagnostic exports.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Move compressed content into a bounded sender without cloning it.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Convenience: whether the declared kind is IDR.
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

        assert_eq!(
            EncodedAccessUnit::new(&l, FrameId::FIRST, idr, gen0(), 0, vec![]),
            Err(LimitsError::ZeroDimension)
        );

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

    fn unit_with_bytes(bytes: Vec<u8>) -> EncodedAccessUnit {
        EncodedAccessUnit::new(
            &ProtocolLimits::ABSOLUTE,
            FrameId::FIRST,
            FrameKind::Idr {
                recovery: RecoveryGeneration::INITIAL,
            },
            gen0(),
            1_000,
            bytes,
        )
        .unwrap()
    }

    #[test]
    fn debug_output_is_independent_of_pixel_content() {
        let first = unit_with_bytes(vec![11, 22, 33, 44]);
        let second = unit_with_bytes(vec![55, 66, 77, 88]);
        assert_ne!(first, second);
        assert_eq!(format!("{first:?}"), format!("{second:?}"));
        assert_eq!(format!("{first:#?}"), format!("{second:#?}"));
        assert_eq!(
            format!("{:?}", Some(&first)),
            format!("{:?}", Some(&second))
        );
        assert!(!format!("{first:?}").contains("11, 22, 33, 44"));
        assert!(format!("{first:?}").contains("byte_len: 4"));
    }

    #[test]
    fn large_payload_debug_output_stays_bounded() {
        let unit = unit_with_bytes(vec![254; 64 * 1024]);
        let compact = format!("{unit:?}");
        let pretty = format!("{unit:#?}");
        assert!(compact.len() < 512);
        assert!(pretty.len() < 512);
        assert!(compact.contains("byte_len: 65536"));
        assert!(!compact.contains("254"));
        assert!(!pretty.contains("254"));
        assert_eq!(unit.bytes().len(), 64 * 1024);
    }
}
