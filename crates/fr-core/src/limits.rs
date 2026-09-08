//! The single protocol/resource limits structure (plan section 17.2).
//!
//! Every ceiling in the system lives here, in one tested place: parsers,
//! allocators, decoder configuration, and FFI boundaries all consult the
//! same negotiated [`ProtocolLimits`] value. The rules:
//!
//! - the [`ABSOLUTE`](ProtocolLimits::ABSOLUTE) limits are implementation
//!   ceilings — protocol maxima, not default operating points, and not
//!   promises of native-resolution support for every monitor;
//! - endpoints and local administrator overrides only ever negotiate
//!   **downward** from them; overrides above a ceiling or below a floor are
//!   typed refusals, never clamped silently;
//! - all length/stride/product arithmetic is checked *before* allocation
//!   and before any foreign call; overflow-adjacent input is a
//!   [`LimitsError`], not a wrapped integer.

use core::error::Error;
use core::fmt;

/// Which limit a refusal is about, for typed error reporting and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LimitField {
    /// Ordinary control-message byte ceiling.
    ControlMessageBytes,
    /// Complete text clipboard item byte ceiling (its own chunked channel).
    ClipboardItemBytes,
    /// Maximum encoded video access-unit bytes.
    EncodedAccessUnitBytes,
    /// Maximum coded dimension per axis, in pixels.
    DimensionPixels,
    /// Maximum coded pixels per picture (width x height).
    CodedPixels,
    /// Reassembly/dependency window, in pictures.
    ReassemblyWindowPictures,
    /// Per-viewer budget for incomplete/held compressed media.
    PerViewerCompressedBytes,
}

impl fmt::Display for LimitField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::ControlMessageBytes => "control-message bytes",
            Self::ClipboardItemBytes => "clipboard-item bytes",
            Self::EncodedAccessUnitBytes => "encoded-access-unit bytes",
            Self::DimensionPixels => "dimension pixels",
            Self::CodedPixels => "coded pixels",
            Self::ReassemblyWindowPictures => "reassembly-window pictures",
            Self::PerViewerCompressedBytes => "per-viewer compressed bytes",
        };
        f.write_str(name)
    }
}

/// Typed refusal from limit validation or checked arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitsError {
    /// A value exceeded the governing ceiling.
    AboveCeiling {
        /// The limit that was exceeded.
        field: LimitField,
        /// The offending value.
        value: u64,
        /// The governing ceiling.
        ceiling: u64,
    },
    /// A value fell below the governing floor.
    BelowFloor {
        /// The limit that was undercut.
        field: LimitField,
        /// The offending value.
        value: u64,
        /// The governing floor.
        floor: u64,
    },
    /// A dimension of zero pixels is never valid.
    ZeroDimension,
    /// Arithmetic on sizes would overflow the checked domain.
    ArithmeticOverflow,
    /// Row alignment must be a power of two (and nonzero).
    InvalidRowAlignment {
        /// The rejected alignment value.
        value: u32,
    },
    /// Bytes-per-pixel outside the supported range.
    InvalidBytesPerPixel {
        /// The rejected bytes-per-pixel value.
        value: u32,
    },
}

impl fmt::Display for LimitsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AboveCeiling {
                field,
                value,
                ceiling,
            } => {
                write!(f, "{value} exceeds the {field} ceiling of {ceiling}")
            }
            Self::BelowFloor {
                field,
                value,
                floor,
            } => {
                write!(f, "{value} is below the {field} floor of {floor}")
            }
            Self::ZeroDimension => f.write_str("zero-pixel dimension"),
            Self::ArithmeticOverflow => f.write_str("size arithmetic would overflow"),
            Self::InvalidRowAlignment { value } => {
                write!(f, "row alignment {value} is not a nonzero power of two")
            }
            Self::InvalidBytesPerPixel { value } => {
                write!(f, "bytes-per-pixel {value} is outside 1..=16")
            }
        }
    }
}

impl Error for LimitsError {}

/// Downward-only overrides applied to [`ProtocolLimits::ABSOLUTE`] by local
/// configuration or peer negotiation. `None` keeps the ceiling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitOverrides {
    /// Override for the control-message ceiling.
    pub max_control_message_bytes: Option<u32>,
    /// Override for the clipboard-item ceiling.
    pub max_clipboard_item_bytes: Option<u32>,
    /// Override for the encoded-access-unit ceiling.
    pub max_encoded_access_unit_bytes: Option<u32>,
    /// Override for the per-axis dimension ceiling.
    pub max_dimension_pixels: Option<u32>,
    /// Override for the coded-pixels-per-picture ceiling.
    pub max_coded_pixels: Option<u64>,
    /// Override for the reassembly window (floor 2, ceiling 12).
    pub reassembly_window_pictures: Option<u8>,
    /// Override for the per-viewer compressed budget.
    pub per_viewer_compressed_bytes: Option<u64>,
}

/// The one limits structure (plan section 17.2). Fields are private so every
/// constructed value is already validated; read through accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolLimits {
    max_control_message_bytes: u32,
    max_clipboard_item_bytes: u32,
    max_encoded_access_unit_bytes: u32,
    max_dimension_pixels: u32,
    max_coded_pixels: u64,
    reassembly_window_pictures: u8,
    per_viewer_compressed_bytes: u64,
}

impl ProtocolLimits {
    /// Floor of the negotiated reassembly window: below two pictures the
    /// repair horizon cannot cover even one in-flight round trip.
    pub const REASSEMBLY_WINDOW_FLOOR: u8 = 2;
    /// Ceiling of the negotiated reassembly window (plan sections 12.3,
    /// 17.2).
    pub const REASSEMBLY_WINDOW_CEILING: u8 = 12;

    /// The implementation ceilings from plan section 17.2.
    pub const ABSOLUTE: Self = Self {
        max_control_message_bytes: 64 * 1024,
        max_clipboard_item_bytes: 1024 * 1024,
        max_encoded_access_unit_bytes: 16 * 1024 * 1024,
        max_dimension_pixels: 8192,
        max_coded_pixels: 16_777_216,
        reassembly_window_pictures: Self::REASSEMBLY_WINDOW_CEILING,
        per_viewer_compressed_bytes: 32 * 1024 * 1024,
    };

    /// Applies downward-only overrides to the absolute ceilings. Values
    /// above a ceiling or below a floor are typed refusals — administrator
    /// convenience never widens an implementation bound (plan section 17.2).
    pub fn with_overrides(overrides: LimitOverrides) -> Result<Self, LimitsError> {
        let a = Self::ABSOLUTE;

        fn take_u32(
            field: LimitField,
            ceiling: u32,
            floor: u32,
            value: Option<u32>,
        ) -> Result<u32, LimitsError> {
            match value {
                None => Ok(ceiling),
                Some(v) if v > ceiling => Err(LimitsError::AboveCeiling {
                    field,
                    value: u64::from(v),
                    ceiling: u64::from(ceiling),
                }),
                Some(v) if v < floor => Err(LimitsError::BelowFloor {
                    field,
                    value: u64::from(v),
                    floor: u64::from(floor),
                }),
                Some(v) => Ok(v),
            }
        }

        let reassembly = match overrides.reassembly_window_pictures {
            None => a.reassembly_window_pictures,
            Some(v) if v > Self::REASSEMBLY_WINDOW_CEILING => {
                return Err(LimitsError::AboveCeiling {
                    field: LimitField::ReassemblyWindowPictures,
                    value: u64::from(v),
                    ceiling: u64::from(Self::REASSEMBLY_WINDOW_CEILING),
                });
            }
            Some(v) if v < Self::REASSEMBLY_WINDOW_FLOOR => {
                return Err(LimitsError::BelowFloor {
                    field: LimitField::ReassemblyWindowPictures,
                    value: u64::from(v),
                    floor: u64::from(Self::REASSEMBLY_WINDOW_FLOOR),
                });
            }
            Some(v) => v,
        };

        // The access-unit ceiling is selected before the per-viewer budget
        // because the budget's floor is relative to the SELECTED ceiling: a
        // deployment that negotiates smaller access units may run a
        // proportionally smaller budget. Flooring against the absolute
        // 16 MiB would wrongly reject small-AU mobile configurations
        // (found in independent review by AzureBasin).
        let max_encoded_access_unit_bytes = take_u32(
            LimitField::EncodedAccessUnitBytes,
            a.max_encoded_access_unit_bytes,
            1,
            overrides.max_encoded_access_unit_bytes,
        )?;

        let per_viewer = match overrides.per_viewer_compressed_bytes {
            None => a.per_viewer_compressed_bytes,
            Some(v) if v > a.per_viewer_compressed_bytes => {
                return Err(LimitsError::AboveCeiling {
                    field: LimitField::PerViewerCompressedBytes,
                    value: v,
                    ceiling: a.per_viewer_compressed_bytes,
                });
            }
            // Floor: a viewer that cannot hold even one access unit at the
            // selected ceiling cannot decode at all.
            Some(v) if v < u64::from(max_encoded_access_unit_bytes) => {
                return Err(LimitsError::BelowFloor {
                    field: LimitField::PerViewerCompressedBytes,
                    value: v,
                    floor: u64::from(max_encoded_access_unit_bytes),
                });
            }
            Some(v) => v,
        };

        let max_coded_pixels = match overrides.max_coded_pixels {
            None => a.max_coded_pixels,
            Some(v) if v > a.max_coded_pixels => {
                return Err(LimitsError::AboveCeiling {
                    field: LimitField::CodedPixels,
                    value: v,
                    ceiling: a.max_coded_pixels,
                });
            }
            Some(0) => return Err(LimitsError::ZeroDimension),
            Some(v) => v,
        };

        Ok(Self {
            max_control_message_bytes: take_u32(
                LimitField::ControlMessageBytes,
                a.max_control_message_bytes,
                1,
                overrides.max_control_message_bytes,
            )?,
            max_clipboard_item_bytes: take_u32(
                LimitField::ClipboardItemBytes,
                a.max_clipboard_item_bytes,
                1,
                overrides.max_clipboard_item_bytes,
            )?,
            max_encoded_access_unit_bytes,
            max_dimension_pixels: take_u32(
                LimitField::DimensionPixels,
                a.max_dimension_pixels,
                1,
                overrides.max_dimension_pixels,
            )?,
            max_coded_pixels,
            reassembly_window_pictures: reassembly,
            per_viewer_compressed_bytes: per_viewer,
        })
    }

    /// The field-wise minimum of two validated limit sets — the negotiated
    /// session limits. Both inputs are already floor-valid, and minima
    /// preserve floors, so the result needs no revalidation.
    #[must_use]
    pub fn negotiated(&self, peer: &Self) -> Self {
        Self {
            max_control_message_bytes: self
                .max_control_message_bytes
                .min(peer.max_control_message_bytes),
            max_clipboard_item_bytes: self
                .max_clipboard_item_bytes
                .min(peer.max_clipboard_item_bytes),
            max_encoded_access_unit_bytes: self
                .max_encoded_access_unit_bytes
                .min(peer.max_encoded_access_unit_bytes),
            max_dimension_pixels: self.max_dimension_pixels.min(peer.max_dimension_pixels),
            max_coded_pixels: self.max_coded_pixels.min(peer.max_coded_pixels),
            reassembly_window_pictures: self
                .reassembly_window_pictures
                .min(peer.reassembly_window_pictures),
            per_viewer_compressed_bytes: self
                .per_viewer_compressed_bytes
                .min(peer.per_viewer_compressed_bytes),
        }
    }

    /// Ceiling accessor: ordinary control-message bytes.
    #[must_use]
    pub const fn max_control_message_bytes(&self) -> u32 {
        self.max_control_message_bytes
    }

    /// Ceiling accessor: clipboard-item bytes.
    #[must_use]
    pub const fn max_clipboard_item_bytes(&self) -> u32 {
        self.max_clipboard_item_bytes
    }

    /// Ceiling accessor: encoded access-unit bytes.
    #[must_use]
    pub const fn max_encoded_access_unit_bytes(&self) -> u32 {
        self.max_encoded_access_unit_bytes
    }

    /// Ceiling accessor: per-axis dimension pixels.
    #[must_use]
    pub const fn max_dimension_pixels(&self) -> u32 {
        self.max_dimension_pixels
    }

    /// Ceiling accessor: coded pixels per picture.
    #[must_use]
    pub const fn max_coded_pixels(&self) -> u64 {
        self.max_coded_pixels
    }

    /// Negotiated reassembly/dependency window in pictures.
    #[must_use]
    pub const fn reassembly_window_pictures(&self) -> u8 {
        self.reassembly_window_pictures
    }

    /// Per-viewer incomplete/held compressed-media budget in bytes.
    #[must_use]
    pub const fn per_viewer_compressed_bytes(&self) -> u64 {
        self.per_viewer_compressed_bytes
    }

    /// Validates an ordinary control-message length before parsing.
    pub fn validate_control_message_len(&self, len: usize) -> Result<(), LimitsError> {
        Self::validate_len(
            LimitField::ControlMessageBytes,
            len,
            u64::from(self.max_control_message_bytes),
        )
    }

    /// Validates a complete clipboard item length before transfer.
    pub fn validate_clipboard_item_len(&self, len: usize) -> Result<(), LimitsError> {
        Self::validate_len(
            LimitField::ClipboardItemBytes,
            len,
            u64::from(self.max_clipboard_item_bytes),
        )
    }

    /// Validates a complete encoded access-unit length before reassembly
    /// admits it.
    pub fn validate_access_unit_len(&self, len: usize) -> Result<(), LimitsError> {
        Self::validate_len(
            LimitField::EncodedAccessUnitBytes,
            len,
            u64::from(self.max_encoded_access_unit_bytes),
        )
    }

    /// Validates coded picture dimensions: each axis within the per-axis
    /// ceiling, nonzero, and the product within the coded-pixels ceiling —
    /// all before any decoder or surface sees them.
    pub fn validate_coded_dimensions(&self, width: u32, height: u32) -> Result<(), LimitsError> {
        if width == 0 || height == 0 {
            return Err(LimitsError::ZeroDimension);
        }
        for axis in [width, height] {
            if axis > self.max_dimension_pixels {
                return Err(LimitsError::AboveCeiling {
                    field: LimitField::DimensionPixels,
                    value: u64::from(axis),
                    ceiling: u64::from(self.max_dimension_pixels),
                });
            }
        }
        let pixels = u64::from(width) * u64::from(height);
        if pixels > self.max_coded_pixels {
            return Err(LimitsError::AboveCeiling {
                field: LimitField::CodedPixels,
                value: pixels,
                ceiling: self.max_coded_pixels,
            });
        }
        Ok(())
    }

    /// Checked surface-size arithmetic: validates the dimensions, then
    /// computes `align_up(width * bytes_per_pixel) * height` without ever
    /// wrapping. `row_align` must be a nonzero power of two;
    /// `bytes_per_pixel` must be in `1..=16`. Alignment padding counts
    /// toward the returned allocation size (plan section 17.2).
    pub fn checked_surface_bytes(
        &self,
        width: u32,
        height: u32,
        bytes_per_pixel: u32,
        row_align: u32,
    ) -> Result<u64, LimitsError> {
        self.validate_coded_dimensions(width, height)?;
        if !(1..=16).contains(&bytes_per_pixel) {
            return Err(LimitsError::InvalidBytesPerPixel {
                value: bytes_per_pixel,
            });
        }
        if row_align == 0 || !row_align.is_power_of_two() {
            return Err(LimitsError::InvalidRowAlignment { value: row_align });
        }
        let row_bytes = u64::from(width)
            .checked_mul(u64::from(bytes_per_pixel))
            .ok_or(LimitsError::ArithmeticOverflow)?;
        let align = u64::from(row_align);
        let stride = row_bytes
            .checked_add(align - 1)
            .ok_or(LimitsError::ArithmeticOverflow)?
            / align
            * align;
        stride
            .checked_mul(u64::from(height))
            .ok_or(LimitsError::ArithmeticOverflow)
    }

    fn validate_len(field: LimitField, len: usize, ceiling: u64) -> Result<(), LimitsError> {
        let value = u64::try_from(len).map_err(|_| LimitsError::ArithmeticOverflow)?;
        if value > ceiling {
            return Err(LimitsError::AboveCeiling {
                field,
                value,
                ceiling,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_ceilings_match_the_plan() {
        let a = ProtocolLimits::ABSOLUTE;
        assert_eq!(a.max_control_message_bytes(), 64 * 1024);
        assert_eq!(a.max_clipboard_item_bytes(), 1024 * 1024);
        assert_eq!(a.max_encoded_access_unit_bytes(), 16 * 1024 * 1024);
        assert_eq!(a.max_dimension_pixels(), 8192);
        assert_eq!(a.max_coded_pixels(), 16_777_216);
        assert_eq!(a.reassembly_window_pictures(), 12);
        assert_eq!(a.per_viewer_compressed_bytes(), 32 * 1024 * 1024);
    }

    #[test]
    fn overrides_only_negotiate_downward() {
        let ok = ProtocolLimits::with_overrides(LimitOverrides {
            max_control_message_bytes: Some(16 * 1024),
            reassembly_window_pictures: Some(4),
            ..LimitOverrides::default()
        })
        .expect("downward overrides are valid");
        assert_eq!(ok.max_control_message_bytes(), 16 * 1024);
        assert_eq!(ok.reassembly_window_pictures(), 4);
        // Untouched fields keep the ceilings.
        assert_eq!(ok.max_coded_pixels(), 16_777_216);

        let above = ProtocolLimits::with_overrides(LimitOverrides {
            max_encoded_access_unit_bytes: Some(16 * 1024 * 1024 + 1),
            ..LimitOverrides::default()
        });
        assert_eq!(
            above,
            Err(LimitsError::AboveCeiling {
                field: LimitField::EncodedAccessUnitBytes,
                value: 16 * 1024 * 1024 + 1,
                ceiling: 16 * 1024 * 1024,
            })
        );
    }

    #[test]
    fn reassembly_window_floor_and_ceiling_hold() {
        for (value, expected) in [
            (
                1_u8,
                Err(LimitsError::BelowFloor {
                    field: LimitField::ReassemblyWindowPictures,
                    value: 1,
                    floor: 2,
                }),
            ),
            (2, Ok(2)),
            (12, Ok(12)),
            (
                13,
                Err(LimitsError::AboveCeiling {
                    field: LimitField::ReassemblyWindowPictures,
                    value: 13,
                    ceiling: 12,
                }),
            ),
        ] {
            let got = ProtocolLimits::with_overrides(LimitOverrides {
                reassembly_window_pictures: Some(value),
                ..LimitOverrides::default()
            })
            .map(|l| l.reassembly_window_pictures());
            assert_eq!(got, expected, "window override {value}");
        }
    }

    #[test]
    fn per_viewer_budget_floor_follows_the_selected_access_unit_ceiling() {
        // A small-AU deployment (e.g. a mobile operating point) may run a
        // proportionally small budget: AU ceiling 1 MiB admits a 2 MiB
        // budget, and refuses only below the SELECTED ceiling, not below
        // the absolute 16 MiB. Regression for the review finding.
        let small = ProtocolLimits::with_overrides(LimitOverrides {
            max_encoded_access_unit_bytes: Some(1024 * 1024),
            per_viewer_compressed_bytes: Some(2 * 1024 * 1024),
            ..LimitOverrides::default()
        })
        .expect("small-AU/small-budget configuration is valid");
        assert_eq!(small.max_encoded_access_unit_bytes(), 1024 * 1024);
        assert_eq!(small.per_viewer_compressed_bytes(), 2 * 1024 * 1024);

        let below_selected = ProtocolLimits::with_overrides(LimitOverrides {
            max_encoded_access_unit_bytes: Some(1024 * 1024),
            per_viewer_compressed_bytes: Some(1024 * 1024 - 1),
            ..LimitOverrides::default()
        });
        assert_eq!(
            below_selected,
            Err(LimitsError::BelowFloor {
                field: LimitField::PerViewerCompressedBytes,
                value: 1024 * 1024 - 1,
                floor: 1024 * 1024,
            })
        );
    }

    #[test]
    fn per_viewer_budget_floor_is_one_maximal_access_unit() {
        let too_small = ProtocolLimits::with_overrides(LimitOverrides {
            per_viewer_compressed_bytes: Some(16 * 1024 * 1024 - 1),
            ..LimitOverrides::default()
        });
        assert!(matches!(
            too_small,
            Err(LimitsError::BelowFloor {
                field: LimitField::PerViewerCompressedBytes,
                ..
            })
        ));
        let exact = ProtocolLimits::with_overrides(LimitOverrides {
            per_viewer_compressed_bytes: Some(16 * 1024 * 1024),
            ..LimitOverrides::default()
        })
        .expect("one maximal access unit is the floor");
        assert_eq!(exact.per_viewer_compressed_bytes(), 16 * 1024 * 1024);
    }

    #[test]
    fn negotiation_takes_the_fieldwise_minimum() {
        let mine = ProtocolLimits::with_overrides(LimitOverrides {
            max_control_message_bytes: Some(32 * 1024),
            reassembly_window_pictures: Some(10),
            ..LimitOverrides::default()
        })
        .unwrap();
        let peer = ProtocolLimits::with_overrides(LimitOverrides {
            max_control_message_bytes: Some(48 * 1024),
            max_clipboard_item_bytes: Some(64 * 1024),
            reassembly_window_pictures: Some(3),
            ..LimitOverrides::default()
        })
        .unwrap();
        let n = mine.negotiated(&peer);
        assert_eq!(n.max_control_message_bytes(), 32 * 1024);
        assert_eq!(n.max_clipboard_item_bytes(), 64 * 1024);
        assert_eq!(n.reassembly_window_pictures(), 3);
        assert_eq!(n.max_coded_pixels(), 16_777_216);
        // Negotiation commutes.
        assert_eq!(n, peer.negotiated(&mine));
    }

    #[test]
    fn length_validators_accept_boundary_and_refuse_above() {
        let l = ProtocolLimits::ABSOLUTE;
        assert!(l.validate_control_message_len(64 * 1024).is_ok());
        assert!(matches!(
            l.validate_control_message_len(64 * 1024 + 1),
            Err(LimitsError::AboveCeiling {
                field: LimitField::ControlMessageBytes,
                ..
            })
        ));
        assert!(l.validate_clipboard_item_len(1024 * 1024).is_ok());
        assert!(l.validate_clipboard_item_len(1024 * 1024 + 1).is_err());
        assert!(l.validate_access_unit_len(16 * 1024 * 1024).is_ok());
        assert!(l.validate_access_unit_len(16 * 1024 * 1024 + 1).is_err());
        assert!(l.validate_control_message_len(0).is_ok());
    }

    #[test]
    fn dimension_validation_enforces_axis_pixels_and_zero_rules() {
        let l = ProtocolLimits::ABSOLUTE;
        // 4096 x 4096 = exactly the 16,777,216-pixel ceiling.
        assert!(l.validate_coded_dimensions(4096, 4096).is_ok());
        // 8192 per axis is legal only while the product stays within the
        // pixel ceiling: 8192 x 2048 passes, 8192 x 2049 does not.
        assert!(l.validate_coded_dimensions(8192, 2048).is_ok());
        assert!(matches!(
            l.validate_coded_dimensions(8192, 2049),
            Err(LimitsError::AboveCeiling {
                field: LimitField::CodedPixels,
                ..
            })
        ));
        assert!(matches!(
            l.validate_coded_dimensions(8193, 1),
            Err(LimitsError::AboveCeiling {
                field: LimitField::DimensionPixels,
                ..
            })
        ));
        assert_eq!(
            l.validate_coded_dimensions(0, 100),
            Err(LimitsError::ZeroDimension)
        );
        assert_eq!(
            l.validate_coded_dimensions(100, 0),
            Err(LimitsError::ZeroDimension)
        );
    }

    #[test]
    fn surface_bytes_are_checked_and_alignment_padding_counts() {
        let l = ProtocolLimits::ABSOLUTE;
        // 100 px * 4 Bpp = 400 bytes/row, aligned to 256 -> 512 stride.
        assert_eq!(l.checked_surface_bytes(100, 10, 4, 256), Ok(512 * 10));
        // Alignment of 1 keeps the exact row size.
        assert_eq!(l.checked_surface_bytes(100, 10, 4, 1), Ok(400 * 10));
        // Invalid alignment and bytes-per-pixel are typed refusals.
        assert_eq!(
            l.checked_surface_bytes(100, 10, 4, 0),
            Err(LimitsError::InvalidRowAlignment { value: 0 })
        );
        assert_eq!(
            l.checked_surface_bytes(100, 10, 4, 3),
            Err(LimitsError::InvalidRowAlignment { value: 3 })
        );
        assert_eq!(
            l.checked_surface_bytes(100, 10, 0, 256),
            Err(LimitsError::InvalidBytesPerPixel { value: 0 })
        );
        assert_eq!(
            l.checked_surface_bytes(100, 10, 17, 256),
            Err(LimitsError::InvalidBytesPerPixel { value: 17 })
        );
        // Over-ceiling dimensions are refused before any arithmetic runs.
        assert!(l.checked_surface_bytes(8193, 1, 4, 256).is_err());
    }

    #[test]
    fn error_display_is_specific_and_loggable() {
        let e = LimitsError::AboveCeiling {
            field: LimitField::CodedPixels,
            value: 33_554_432,
            ceiling: 16_777_216,
        };
        assert_eq!(
            e.to_string(),
            "33554432 exceeds the coded pixels ceiling of 16777216"
        );
        let f = LimitsError::BelowFloor {
            field: LimitField::ReassemblyWindowPictures,
            value: 1,
            floor: 2,
        };
        assert_eq!(
            f.to_string(),
            "1 is below the reassembly-window pictures floor of 2"
        );
    }
}
