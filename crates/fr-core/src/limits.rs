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
    /// Maximum concurrent pre-admission/handshake connections.
    ConcurrentHandshakes,
    /// Maximum duration of a handshake attempt in milliseconds.
    HandshakeDurationMs,
    /// Maximum pre-admission connection rate per second.
    PreadmissionRatePerSec,
    /// Maximum concurrent half-attached channels.
    HalfAttachedChannels,
    /// Maximum concurrent pending local authorization requests.
    PendingApprovals,
    /// Idle session timeout in seconds.
    IdleSessionTimeoutSecs,
    /// Maximum rate of ordinary control requests per second.
    ControlRequestsPerSec,
    /// Maximum rate of expensive codec probes per minute.
    CodecProbesPerMin,
    /// Maximum rate of recovery requests per second.
    RecoveryRequestsPerSec,
    /// Maximum rate of cursor shape uploads per second.
    CursorUploadsPerSec,
    /// Maximum rate of diagnostic exports per minute.
    DiagnosticExportsPerMin,
    /// Maximum decoder reconfigurations per minute.
    DecoderReconfigurationsPerMin,
    /// Maximum worker restarts per minute.
    WorkerRestartsPerMin,
    /// Maximum cursor dimension per axis in pixels.
    CursorDimensionPixels,
    /// Maximum cursor shape upload bytes.
    CursorShapeBytes,
    /// Maximum Unicode name byte length.
    NameBytes,
    /// Maximum parameter set (VPS/SPS/PPS) byte length.
    ParameterSetBytes,
    /// Maximum number of metadata fragments per access unit.
    FragmentsPerAccessUnit,
    /// Maximum retained request receipts in sequence ledger.
    RetainedReceipts,
    /// Maximum concurrent active encoder sessions.
    EncoderSessions,
    /// Maximum concurrent allocated GPU surfaces.
    GpuSurfaces,
    /// Maximum outbound bandwidth in bits per second.
    BandwidthBps,
    /// Maximum concurrent viewers.
    Viewers,
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
            Self::ConcurrentHandshakes => "concurrent handshakes",
            Self::HandshakeDurationMs => "handshake duration ms",
            Self::PreadmissionRatePerSec => "preadmission rate per sec",
            Self::HalfAttachedChannels => "half-attached channels",
            Self::PendingApprovals => "pending approvals",
            Self::IdleSessionTimeoutSecs => "idle session timeout seconds",
            Self::ControlRequestsPerSec => "control requests per sec",
            Self::CodecProbesPerMin => "codec probes per min",
            Self::RecoveryRequestsPerSec => "recovery requests per sec",
            Self::CursorUploadsPerSec => "cursor uploads per sec",
            Self::DiagnosticExportsPerMin => "diagnostic exports per min",
            Self::DecoderReconfigurationsPerMin => "decoder reconfigurations per min",
            Self::WorkerRestartsPerMin => "worker restarts per min",
            Self::CursorDimensionPixels => "cursor dimension pixels",
            Self::CursorShapeBytes => "cursor shape bytes",
            Self::NameBytes => "name bytes",
            Self::ParameterSetBytes => "parameter-set bytes",
            Self::FragmentsPerAccessUnit => "fragments per access unit",
            Self::RetainedReceipts => "retained receipts",
            Self::EncoderSessions => "encoder sessions",
            Self::GpuSurfaces => "gpu surfaces",
            Self::BandwidthBps => "bandwidth bps",
            Self::Viewers => "viewers",
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
    /// Override for maximum concurrent pre-admission/handshake attempts.
    pub max_concurrent_handshakes: Option<u32>,
    /// Override for maximum handshake duration in milliseconds.
    pub max_handshake_duration_ms: Option<u32>,
    /// Override for maximum pre-admission rate per second.
    pub max_preadmission_rate_per_sec: Option<u32>,
    /// Override for maximum concurrent half-attached channels.
    pub max_half_attached_channels: Option<u32>,
    /// Override for maximum concurrent pending local approvals.
    pub max_pending_approvals: Option<u32>,
    /// Override for idle session timeout in seconds.
    pub idle_session_timeout_seconds: Option<u32>,
    /// Override for maximum control requests per second.
    pub max_control_requests_per_sec: Option<u32>,
    /// Override for maximum codec probes per minute.
    pub max_codec_probes_per_min: Option<u32>,
    /// Override for maximum recovery requests per second.
    pub max_recovery_requests_per_sec: Option<u32>,
    /// Override for maximum cursor uploads per second.
    pub max_cursor_uploads_per_sec: Option<u32>,
    /// Override for maximum diagnostic exports per minute.
    pub max_diagnostic_exports_per_min: Option<u32>,
    /// Override for maximum decoder reconfigurations per minute.
    pub max_decoder_reconfigurations_per_min: Option<u32>,
    /// Override for maximum worker restarts per minute.
    pub max_worker_restarts_per_min: Option<u32>,
    /// Override for maximum cursor dimension per axis in pixels.
    pub max_cursor_dimension_pixels: Option<u32>,
    /// Override for maximum cursor shape bytes.
    pub max_cursor_shape_bytes: Option<u32>,
    /// Override for maximum Unicode name bytes.
    pub max_name_bytes: Option<usize>,
    /// Override for maximum parameter set bytes.
    pub max_parameter_set_bytes: Option<u32>,
    /// Override for maximum metadata fragments per access unit.
    pub max_fragments_per_access_unit: Option<u32>,
    /// Override for maximum retained request receipts.
    pub max_retained_receipts: Option<usize>,
    /// Override for maximum concurrent encoder sessions.
    pub max_encoder_sessions: Option<u32>,
    /// Override for maximum concurrent GPU surfaces.
    pub max_gpu_surfaces: Option<u32>,
    /// Override for maximum bandwidth in bits per second.
    pub max_bandwidth_bps: Option<u64>,
    /// Override for maximum concurrent viewers.
    pub max_viewers: Option<u32>,
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
    max_concurrent_handshakes: u16,
    max_handshake_duration_ms: u16,
    max_preadmission_rate_per_sec: u16,
    max_half_attached_channels: u16,
    max_pending_approvals: u8,
    idle_session_timeout_seconds: u16,
    max_control_requests_per_sec: u16,
    max_codec_probes_per_min: u16,
    max_recovery_requests_per_sec: u16,
    max_cursor_uploads_per_sec: u16,
    max_diagnostic_exports_per_min: u8,
    max_decoder_reconfigurations_per_min: u16,
    max_worker_restarts_per_min: u8,
    max_cursor_dimension_pixels: u16,
    max_cursor_shape_bytes: u32,
    max_name_bytes: u16,
    max_parameter_set_bytes: u32,
    max_fragments_per_access_unit: u16,
    max_retained_receipts: u16,
    max_encoder_sessions: u8,
    max_gpu_surfaces: u8,
    max_bandwidth_bps: u64,
    max_viewers: u8,
}

macro_rules! val_cap {
    ($field:ident, $val:expr, $cap:expr) => {
        Self::validate_capacity(LimitField::$field, u64::from($val), u64::from($cap))
    };
}
macro_rules! val_max {
    ($field:ident, $val:expr, $max:expr) => {
        Self::validate_max(LimitField::$field, u64::from($val), u64::from($max))
    };
}
macro_rules! val_len {
    ($field:ident, $len:expr, $max:expr) => {
        Self::validate_len(LimitField::$field, $len, u64::from($max))
    };
}

impl ProtocolLimits {
    /// Floor of the negotiated reassembly window: below two pictures the
    /// repair horizon cannot cover even one in-flight round trip.
    pub const REASSEMBLY_WINDOW_FLOOR: u8 = 2;
    /// Ceiling of the negotiated reassembly window (plan sections 12.3,
    /// 17.2).
    pub const REASSEMBLY_WINDOW_CEILING: u8 = 12;

    /// The implementation ceilings from plan section 17.2 and 19.3.
    pub const ABSOLUTE: Self = Self {
        max_control_message_bytes: 64 * 1024,
        max_clipboard_item_bytes: 1024 * 1024,
        max_encoded_access_unit_bytes: 16 * 1024 * 1024,
        max_dimension_pixels: 8192,
        max_coded_pixels: 16_777_216,
        reassembly_window_pictures: Self::REASSEMBLY_WINDOW_CEILING,
        per_viewer_compressed_bytes: 32 * 1024 * 1024,
        max_concurrent_handshakes: 64,
        max_handshake_duration_ms: 10_000,
        max_preadmission_rate_per_sec: 100,
        max_half_attached_channels: 16,
        max_pending_approvals: 8,
        idle_session_timeout_seconds: 300,
        max_control_requests_per_sec: 200,
        max_codec_probes_per_min: 30,
        max_recovery_requests_per_sec: 120,
        max_cursor_uploads_per_sec: 60,
        max_diagnostic_exports_per_min: 10,
        max_decoder_reconfigurations_per_min: 30,
        max_worker_restarts_per_min: 10,
        max_cursor_dimension_pixels: 256,
        max_cursor_shape_bytes: 262_144,
        max_name_bytes: 256,
        max_parameter_set_bytes: 64 * 1024,
        max_fragments_per_access_unit: 4096,
        max_retained_receipts: 1024,
        max_encoder_sessions: 4,
        max_gpu_surfaces: 32,
        max_bandwidth_bps: 1_000_000_000,
        max_viewers: 3,
    };

    /// Applies downward-only overrides to the absolute ceilings. Values
    /// above a ceiling or below a floor are typed refusals — administrator
    /// convenience never widens an implementation bound (plan section 17.2).
    pub fn with_overrides(overrides: LimitOverrides) -> Result<Self, LimitsError> {
        let a = Self::ABSOLUTE;
        let session = resolve_session_overrides(&overrides, &a)?;
        let rates = resolve_rate_overrides(&overrides, &a)?;
        let resources = resolve_resource_overrides(&overrides, &a)?;

        Ok(Self {
            max_control_message_bytes: session.max_control_message_bytes,
            max_clipboard_item_bytes: session.max_clipboard_item_bytes,
            max_encoded_access_unit_bytes: session.max_encoded_access_unit_bytes,
            max_dimension_pixels: session.max_dimension_pixels,
            max_coded_pixels: session.max_coded_pixels,
            reassembly_window_pictures: session.reassembly_window_pictures,
            per_viewer_compressed_bytes: session.per_viewer_compressed_bytes,
            max_concurrent_handshakes: rates.max_concurrent_handshakes,
            max_handshake_duration_ms: rates.max_handshake_duration_ms,
            max_preadmission_rate_per_sec: rates.max_preadmission_rate_per_sec,
            max_half_attached_channels: rates.max_half_attached_channels,
            max_pending_approvals: rates.max_pending_approvals,
            idle_session_timeout_seconds: rates.idle_session_timeout_seconds,
            max_control_requests_per_sec: rates.max_control_requests_per_sec,
            max_codec_probes_per_min: rates.max_codec_probes_per_min,
            max_recovery_requests_per_sec: rates.max_recovery_requests_per_sec,
            max_cursor_uploads_per_sec: rates.max_cursor_uploads_per_sec,
            max_diagnostic_exports_per_min: rates.max_diagnostic_exports_per_min,
            max_decoder_reconfigurations_per_min: rates.max_decoder_reconfigurations_per_min,
            max_worker_restarts_per_min: rates.max_worker_restarts_per_min,
            max_cursor_dimension_pixels: resources.cursor_dimension_pixels,
            max_cursor_shape_bytes: resources.cursor_shape_bytes,
            max_name_bytes: resources.name_bytes,
            max_parameter_set_bytes: resources.parameter_set_bytes,
            max_fragments_per_access_unit: resources.fragments_per_access_unit,
            max_retained_receipts: resources.retained_receipts,
            max_encoder_sessions: resources.encoder_sessions,
            max_gpu_surfaces: resources.gpu_surfaces,
            max_bandwidth_bps: resources.bandwidth_bps,
            max_viewers: resources.viewers,
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
            max_concurrent_handshakes: self
                .max_concurrent_handshakes
                .min(peer.max_concurrent_handshakes),
            max_handshake_duration_ms: self
                .max_handshake_duration_ms
                .min(peer.max_handshake_duration_ms),
            max_preadmission_rate_per_sec: self
                .max_preadmission_rate_per_sec
                .min(peer.max_preadmission_rate_per_sec),
            max_half_attached_channels: self
                .max_half_attached_channels
                .min(peer.max_half_attached_channels),
            max_pending_approvals: self.max_pending_approvals.min(peer.max_pending_approvals),
            idle_session_timeout_seconds: self
                .idle_session_timeout_seconds
                .min(peer.idle_session_timeout_seconds),
            max_control_requests_per_sec: self
                .max_control_requests_per_sec
                .min(peer.max_control_requests_per_sec),
            max_codec_probes_per_min: self
                .max_codec_probes_per_min
                .min(peer.max_codec_probes_per_min),
            max_recovery_requests_per_sec: self
                .max_recovery_requests_per_sec
                .min(peer.max_recovery_requests_per_sec),
            max_cursor_uploads_per_sec: self
                .max_cursor_uploads_per_sec
                .min(peer.max_cursor_uploads_per_sec),
            max_diagnostic_exports_per_min: self
                .max_diagnostic_exports_per_min
                .min(peer.max_diagnostic_exports_per_min),
            max_decoder_reconfigurations_per_min: self
                .max_decoder_reconfigurations_per_min
                .min(peer.max_decoder_reconfigurations_per_min),
            max_worker_restarts_per_min: self
                .max_worker_restarts_per_min
                .min(peer.max_worker_restarts_per_min),
            max_cursor_dimension_pixels: self
                .max_cursor_dimension_pixels
                .min(peer.max_cursor_dimension_pixels),
            max_cursor_shape_bytes: self.max_cursor_shape_bytes.min(peer.max_cursor_shape_bytes),
            max_name_bytes: self.max_name_bytes.min(peer.max_name_bytes),
            max_parameter_set_bytes: self
                .max_parameter_set_bytes
                .min(peer.max_parameter_set_bytes),
            max_fragments_per_access_unit: self
                .max_fragments_per_access_unit
                .min(peer.max_fragments_per_access_unit),
            max_retained_receipts: self.max_retained_receipts.min(peer.max_retained_receipts),
            max_encoder_sessions: self.max_encoder_sessions.min(peer.max_encoder_sessions),
            max_gpu_surfaces: self.max_gpu_surfaces.min(peer.max_gpu_surfaces),
            max_bandwidth_bps: self.max_bandwidth_bps.min(peer.max_bandwidth_bps),
            max_viewers: self.max_viewers.min(peer.max_viewers),
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

    /// Maximum concurrent pre-admission/handshake attempts.
    #[must_use]
    pub const fn max_concurrent_handshakes(&self) -> u32 {
        self.max_concurrent_handshakes as u32
    }

    /// Maximum handshake duration in milliseconds.
    #[must_use]
    pub const fn max_handshake_duration_ms(&self) -> u32 {
        self.max_handshake_duration_ms as u32
    }

    /// Maximum pre-admission rate per second.
    #[must_use]
    pub const fn max_preadmission_rate_per_sec(&self) -> u32 {
        self.max_preadmission_rate_per_sec as u32
    }

    /// Maximum concurrent half-attached channels.
    #[must_use]
    pub const fn max_half_attached_channels(&self) -> u32 {
        self.max_half_attached_channels as u32
    }

    /// Maximum concurrent pending local approvals.
    #[must_use]
    pub const fn max_pending_approvals(&self) -> u32 {
        self.max_pending_approvals as u32
    }

    /// Idle session timeout in seconds.
    #[must_use]
    pub const fn idle_session_timeout_seconds(&self) -> u32 {
        self.idle_session_timeout_seconds as u32
    }

    /// Maximum control requests per second.
    #[must_use]
    pub const fn max_control_requests_per_sec(&self) -> u32 {
        self.max_control_requests_per_sec as u32
    }

    /// Maximum codec probes per minute.
    #[must_use]
    pub const fn max_codec_probes_per_min(&self) -> u32 {
        self.max_codec_probes_per_min as u32
    }

    /// Maximum recovery requests per second.
    #[must_use]
    pub const fn max_recovery_requests_per_sec(&self) -> u32 {
        self.max_recovery_requests_per_sec as u32
    }

    /// Maximum cursor uploads per second.
    #[must_use]
    pub const fn max_cursor_uploads_per_sec(&self) -> u32 {
        self.max_cursor_uploads_per_sec as u32
    }

    /// Maximum diagnostic exports per minute.
    #[must_use]
    pub const fn max_diagnostic_exports_per_min(&self) -> u32 {
        self.max_diagnostic_exports_per_min as u32
    }

    /// Maximum decoder reconfigurations per minute.
    #[must_use]
    pub const fn max_decoder_reconfigurations_per_min(&self) -> u32 {
        self.max_decoder_reconfigurations_per_min as u32
    }

    /// Maximum worker restarts per minute.
    #[must_use]
    pub const fn max_worker_restarts_per_min(&self) -> u32 {
        self.max_worker_restarts_per_min as u32
    }

    /// Maximum cursor dimension per axis in pixels.
    #[must_use]
    pub const fn max_cursor_dimension_pixels(&self) -> u32 {
        self.max_cursor_dimension_pixels as u32
    }

    /// Maximum cursor shape bytes.
    #[must_use]
    pub const fn max_cursor_shape_bytes(&self) -> u32 {
        self.max_cursor_shape_bytes
    }

    /// Maximum Unicode name bytes.
    #[must_use]
    pub const fn max_name_bytes(&self) -> usize {
        self.max_name_bytes as usize
    }

    /// Maximum parameter set bytes.
    #[must_use]
    pub const fn max_parameter_set_bytes(&self) -> u32 {
        self.max_parameter_set_bytes
    }

    /// Maximum metadata fragments per access unit.
    #[must_use]
    pub const fn max_fragments_per_access_unit(&self) -> u32 {
        self.max_fragments_per_access_unit as u32
    }

    /// Maximum retained request receipts.
    #[must_use]
    pub const fn max_retained_receipts(&self) -> usize {
        self.max_retained_receipts as usize
    }

    /// Maximum concurrent encoder sessions.
    #[must_use]
    pub const fn max_encoder_sessions(&self) -> u32 {
        self.max_encoder_sessions as u32
    }

    /// Maximum concurrent GPU surfaces.
    #[must_use]
    pub const fn max_gpu_surfaces(&self) -> u32 {
        self.max_gpu_surfaces as u32
    }

    /// Maximum bandwidth in bits per second.
    #[must_use]
    pub const fn max_bandwidth_bps(&self) -> u64 {
        self.max_bandwidth_bps
    }

    /// Maximum concurrent viewers.
    #[must_use]
    pub const fn max_viewers(&self) -> u32 {
        self.max_viewers as u32
    }

    /// Validates an ordinary control-message length before parsing.
    pub fn validate_control_message_len(&self, len: usize) -> Result<(), LimitsError> {
        val_len!(ControlMessageBytes, len, self.max_control_message_bytes)
    }

    /// Validates a complete clipboard item length before transfer.
    pub fn validate_clipboard_item_len(&self, len: usize) -> Result<(), LimitsError> {
        val_len!(ClipboardItemBytes, len, self.max_clipboard_item_bytes)
    }

    /// Validates a complete encoded access-unit length before reassembly
    /// admits it.
    pub fn validate_access_unit_len(&self, len: usize) -> Result<(), LimitsError> {
        val_len!(
            EncodedAccessUnitBytes,
            len,
            self.max_encoded_access_unit_bytes
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

    /// Validates cursor dimensions before uploading or decoding a cursor shape.
    pub fn validate_cursor_dimensions(&self, width: u32, height: u32) -> Result<(), LimitsError> {
        if width == 0 || height == 0 {
            return Err(LimitsError::ZeroDimension);
        }
        for axis in [width, height] {
            if axis > u32::from(self.max_cursor_dimension_pixels) {
                return Err(LimitsError::AboveCeiling {
                    field: LimitField::CursorDimensionPixels,
                    value: u64::from(axis),
                    ceiling: u64::from(self.max_cursor_dimension_pixels),
                });
            }
        }
        Ok(())
    }

    /// Validates cursor shape payload length before allocation or FFI.
    pub fn validate_cursor_shape_len(&self, len: usize) -> Result<(), LimitsError> {
        val_len!(CursorShapeBytes, len, self.max_cursor_shape_bytes)
    }

    /// Validates a Unicode name byte length before allocation or display.
    pub fn validate_name_len(&self, len: usize) -> Result<(), LimitsError> {
        Self::validate_len(LimitField::NameBytes, len, u64::from(self.max_name_bytes))
    }

    /// Validates parameter set (VPS/SPS/PPS) byte length before parsing.
    pub fn validate_parameter_set_len(&self, len: usize) -> Result<(), LimitsError> {
        val_len!(ParameterSetBytes, len, self.max_parameter_set_bytes)
    }

    fn validate_max(field: LimitField, count: u64, max: u64) -> Result<(), LimitsError> {
        if count > max {
            return Err(LimitsError::AboveCeiling {
                field,
                value: count,
                ceiling: max,
            });
        }
        Ok(())
    }

    fn validate_capacity(field: LimitField, count: u64, cap: u64) -> Result<(), LimitsError> {
        if count >= cap {
            return Err(LimitsError::AboveCeiling {
                field,
                value: count,
                ceiling: cap,
            });
        }
        Ok(())
    }

    /// Validates access-unit metadata fragment count against the fragment ceiling.
    pub fn validate_fragment_count(&self, count: u32) -> Result<(), LimitsError> {
        val_max!(
            FragmentsPerAccessUnit,
            count,
            self.max_fragments_per_access_unit
        )
    }

    /// Validates concurrent handshake count before admitting another.
    pub fn validate_handshake_concurrency(&self, count: u32) -> Result<(), LimitsError> {
        val_cap!(ConcurrentHandshakes, count, self.max_concurrent_handshakes)
    }

    /// Validates pending approvals count before queuing another.
    pub fn validate_pending_approvals(&self, count: u32) -> Result<(), LimitsError> {
        val_cap!(PendingApprovals, count, self.max_pending_approvals)
    }

    /// Validates half-attached channels count before admitting another.
    pub fn validate_half_attached_channels(&self, count: u32) -> Result<(), LimitsError> {
        val_cap!(HalfAttachedChannels, count, self.max_half_attached_channels)
    }

    /// Validates retained receipts count against the ledger ceiling.
    pub fn validate_retained_receipts(&self, count: usize) -> Result<(), LimitsError> {
        val_max!(
            RetainedReceipts,
            u64::try_from(count).unwrap_or(u64::MAX),
            self.max_retained_receipts
        )
    }

    /// Validates encoder session count before launching a new encoder.
    pub fn validate_encoder_sessions(&self, count: u32) -> Result<(), LimitsError> {
        val_cap!(EncoderSessions, count, self.max_encoder_sessions)
    }

    /// Validates GPU surfaces count before allocating additional surfaces.
    pub fn validate_gpu_surfaces(&self, count: u32) -> Result<(), LimitsError> {
        val_cap!(GpuSurfaces, count, self.max_gpu_surfaces)
    }

    /// Validates viewer count before admitting another viewer.
    pub fn validate_viewers(&self, count: u32) -> Result<(), LimitsError> {
        val_cap!(Viewers, count, self.max_viewers)
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

struct SessionOverrides {
    max_control_message_bytes: u32,
    max_clipboard_item_bytes: u32,
    max_encoded_access_unit_bytes: u32,
    max_dimension_pixels: u32,
    max_coded_pixels: u64,
    reassembly_window_pictures: u8,
    per_viewer_compressed_bytes: u64,
}

struct RateOverrides {
    max_concurrent_handshakes: u16,
    max_handshake_duration_ms: u16,
    max_preadmission_rate_per_sec: u16,
    max_half_attached_channels: u16,
    max_pending_approvals: u8,
    idle_session_timeout_seconds: u16,
    max_control_requests_per_sec: u16,
    max_codec_probes_per_min: u16,
    max_recovery_requests_per_sec: u16,
    max_cursor_uploads_per_sec: u16,
    max_diagnostic_exports_per_min: u8,
    max_decoder_reconfigurations_per_min: u16,
    max_worker_restarts_per_min: u8,
}

struct ResourceOverrides {
    cursor_dimension_pixels: u16,
    cursor_shape_bytes: u32,
    name_bytes: u16,
    parameter_set_bytes: u32,
    fragments_per_access_unit: u16,
    retained_receipts: u16,
    encoder_sessions: u8,
    gpu_surfaces: u8,
    bandwidth_bps: u64,
    viewers: u8,
}

fn take_bounded<T: Copy + Into<u64> + TryFrom<u64>, V: Copy + TryInto<u64>>(
    field: LimitField,
    ceiling: T,
    floor: T,
    value: Option<V>,
) -> Result<T, LimitsError> {
    let ceil_u64 = ceiling.into();
    let floor_u64 = floor.into();
    match value {
        None => Ok(ceiling),
        Some(v) => {
            let v_u64 = v.try_into().map_err(|_| LimitsError::ArithmeticOverflow)?;
            if v_u64 > ceil_u64 {
                Err(LimitsError::AboveCeiling {
                    field,
                    value: v_u64,
                    ceiling: ceil_u64,
                })
            } else if v_u64 < floor_u64 {
                Err(LimitsError::BelowFloor {
                    field,
                    value: v_u64,
                    floor: floor_u64,
                })
            } else {
                T::try_from(v_u64).map_err(|_| LimitsError::ArithmeticOverflow)
            }
        }
    }
}

macro_rules! take_l {
    ($f:ident, $field:ident, $floor:expr, $a:expr, $o:expr) => {
        take_bounded(LimitField::$f, $a.$field, $floor, $o.$field)
    };
}

fn resolve_session_overrides(
    overrides: &LimitOverrides,
    a: &ProtocolLimits,
) -> Result<SessionOverrides, LimitsError> {
    let reassembly = match overrides.reassembly_window_pictures {
        None => a.reassembly_window_pictures,
        Some(v) if v > ProtocolLimits::REASSEMBLY_WINDOW_CEILING => {
            return Err(LimitsError::AboveCeiling {
                field: LimitField::ReassemblyWindowPictures,
                value: u64::from(v),
                ceiling: u64::from(ProtocolLimits::REASSEMBLY_WINDOW_CEILING),
            });
        }
        Some(v) if v < ProtocolLimits::REASSEMBLY_WINDOW_FLOOR => {
            return Err(LimitsError::BelowFloor {
                field: LimitField::ReassemblyWindowPictures,
                value: u64::from(v),
                floor: u64::from(ProtocolLimits::REASSEMBLY_WINDOW_FLOOR),
            });
        }
        Some(v) => v,
    };

    let max_encoded_access_unit_bytes = take_l!(
        EncodedAccessUnitBytes,
        max_encoded_access_unit_bytes,
        1,
        a,
        overrides
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

    Ok(SessionOverrides {
        max_control_message_bytes: take_l!(
            ControlMessageBytes,
            max_control_message_bytes,
            1,
            a,
            overrides
        )?,
        max_clipboard_item_bytes: take_l!(
            ClipboardItemBytes,
            max_clipboard_item_bytes,
            1,
            a,
            overrides
        )?,
        max_encoded_access_unit_bytes,
        max_dimension_pixels: take_l!(DimensionPixels, max_dimension_pixels, 1, a, overrides)?,
        max_coded_pixels,
        reassembly_window_pictures: reassembly,
        per_viewer_compressed_bytes: per_viewer,
    })
}

#[rustfmt::skip]
fn resolve_rate_overrides(
    o: &LimitOverrides,
    a: &ProtocolLimits,
) -> Result<RateOverrides, LimitsError> {
    Ok(RateOverrides {
        max_concurrent_handshakes: take_l!(ConcurrentHandshakes, max_concurrent_handshakes, 1, a, o)?,
        max_handshake_duration_ms: take_l!(HandshakeDurationMs, max_handshake_duration_ms, 1_000, a, o)?,
        max_preadmission_rate_per_sec: take_l!(PreadmissionRatePerSec, max_preadmission_rate_per_sec, 1, a, o)?,
        max_half_attached_channels: take_l!(HalfAttachedChannels, max_half_attached_channels, 1, a, o)?,
        max_pending_approvals: take_l!(PendingApprovals, max_pending_approvals, 1, a, o)?,
        idle_session_timeout_seconds: take_l!(IdleSessionTimeoutSecs, idle_session_timeout_seconds, 10, a, o)?,
        max_control_requests_per_sec: take_l!(ControlRequestsPerSec, max_control_requests_per_sec, 10, a, o)?,
        max_codec_probes_per_min: take_l!(CodecProbesPerMin, max_codec_probes_per_min, 1, a, o)?,
        max_recovery_requests_per_sec: take_l!(RecoveryRequestsPerSec, max_recovery_requests_per_sec, 10, a, o)?,
        max_cursor_uploads_per_sec: take_l!(CursorUploadsPerSec, max_cursor_uploads_per_sec, 1, a, o)?,
        max_diagnostic_exports_per_min: take_l!(DiagnosticExportsPerMin, max_diagnostic_exports_per_min, 1, a, o)?,
        max_decoder_reconfigurations_per_min: take_l!(DecoderReconfigurationsPerMin, max_decoder_reconfigurations_per_min, 1, a, o)?,
        max_worker_restarts_per_min: take_l!(WorkerRestartsPerMin, max_worker_restarts_per_min, 1, a, o)?,
    })
}

#[rustfmt::skip]
fn resolve_resource_overrides(
    o: &LimitOverrides,
    a: &ProtocolLimits,
) -> Result<ResourceOverrides, LimitsError> {
    Ok(ResourceOverrides {
        cursor_dimension_pixels: take_l!(CursorDimensionPixels, max_cursor_dimension_pixels, 16, a, o)?,
        cursor_shape_bytes: take_l!(CursorShapeBytes, max_cursor_shape_bytes, 1024, a, o)?,
        name_bytes: take_l!(NameBytes, max_name_bytes, 1, a, o)?,
        parameter_set_bytes: take_l!(ParameterSetBytes, max_parameter_set_bytes, 32, a, o)?,
        fragments_per_access_unit: take_l!(FragmentsPerAccessUnit, max_fragments_per_access_unit, 1, a, o)?,
        retained_receipts: take_l!(RetainedReceipts, max_retained_receipts, 16, a, o)?,
        encoder_sessions: take_l!(EncoderSessions, max_encoder_sessions, 1, a, o)?,
        gpu_surfaces: take_l!(GpuSurfaces, max_gpu_surfaces, 2, a, o)?,
        bandwidth_bps: take_l!(BandwidthBps, max_bandwidth_bps, 1_000_000, a, o)?,
        viewers: take_l!(Viewers, max_viewers, 1, a, o)?,
    })
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
