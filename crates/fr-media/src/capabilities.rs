//! Probed capabilities, the device-identity-keyed probe cache, and session
//! admission (plan sections 8.2, 13.3, 19.3).
//!
//! Capability is never inferred from a vendor name or an API accepting a
//! request: a [`MediaCapabilities`] value is the *result* of a real
//! encode/decode probe of a representative workload, recorded against the
//! exact device identity it was measured on. The [`ProbeCache`] keys results
//! by [`DeviceIdentity`] and invalidates on device loss, so a stale result
//! never authorises a session after a driver change. [`SessionAdmission`]
//! then admits each live encoder/decoder session against current capacity —
//! probing that a device *can* do something is not permission for an
//! unbounded number of concurrent sessions to do it.

use core::fmt;
use std::error::Error;

use crate::config::CodecProfile;
use crate::surface::{CopyLedger, PixelFormat};

/// Maximum number of distinct device probe results retained in the cache.
/// Prevents unbounded memory growth across arbitrary device enumerations (AGENTS.md section 5).
pub const MAX_PROBE_CACHE_ENTRIES: usize = 16;

/// Maximum byte length for device identity strings (OS build, device, driver).
pub const MAX_IDENTITY_FIELD_LEN: usize = 128;

/// Maximum number of probed profiles retained per capability record.
pub const MAX_PROBED_PROFILES: usize = 16;

/// Maximum number of accepted pixel formats retained per capability record.
pub const MAX_PROBED_FORMATS: usize = 16;

/// Typed refusal when constructing or inserting capability/identity metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapabilityError {
    /// A device identity string exceeded [`MAX_IDENTITY_FIELD_LEN`].
    IdentityFieldTooLong {
        /// Name of the offending field.
        field: &'static str,
        /// Actual byte length.
        len: usize,
        /// Maximum allowed byte length.
        max: usize,
    },
    /// A device identity string was empty.
    IdentityFieldEmpty {
        /// Name of the offending field.
        field: &'static str,
    },
    /// Probed profiles collection exceeded [`MAX_PROBED_PROFILES`].
    TooManyProfiles {
        /// Actual count.
        count: usize,
        /// Maximum allowed.
        max: usize,
    },
    /// Probed accepted formats collection exceeded [`MAX_PROBED_FORMATS`].
    TooManyFormats {
        /// Actual count.
        count: usize,
        /// Maximum allowed.
        max: usize,
    },
    /// Zero-pixel dimension is invalid.
    ZeroDimension,
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentityFieldTooLong { field, len, max } => {
                write!(
                    f,
                    "identity field '{field}' length {len} exceeds ceiling of {max} bytes"
                )
            }
            Self::IdentityFieldEmpty { field } => {
                write!(f, "identity field '{field}' must not be empty")
            }
            Self::TooManyProfiles { count, max } => {
                write!(f, "probed profiles count {count} exceeds ceiling of {max}")
            }
            Self::TooManyFormats { count, max } => {
                write!(f, "accepted formats count {count} exceeds ceiling of {max}")
            }
            Self::ZeroDimension => f.write_str("probed dimensions must be nonzero"),
        }
    }
}

impl Error for CapabilityError {}

fn validate_identity_field(name: &'static str, val: &str) -> Result<(), CapabilityError> {
    if val.is_empty() {
        return Err(CapabilityError::IdentityFieldEmpty { field: name });
    }
    if val.len() > MAX_IDENTITY_FIELD_LEN {
        return Err(CapabilityError::IdentityFieldTooLong {
            field: name,
            len: val.len(),
            max: MAX_IDENTITY_FIELD_LEN,
        });
    }
    Ok(())
}

/// The exact hardware/software identity a probe result is bound to. A result
/// is only valid while every field still matches the live device; any change
/// (driver update, GPU swap, OS build) invalidates it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceIdentity {
    /// OS build string.
    pub os_build: String,
    /// GPU/adapter identifier.
    pub device: String,
    /// Driver version string.
    pub driver: String,
    /// A monotonically increasing generation the platform bumps on device
    /// loss/reset, so a reset with otherwise-identical strings still
    /// invalidates the cached result.
    pub reset_generation: u64,
}

impl DeviceIdentity {
    /// Constructs and validates a new device identity.
    pub fn new(
        os_build: impl Into<String>,
        device: impl Into<String>,
        driver: impl Into<String>,
        reset_generation: u64,
    ) -> Result<Self, CapabilityError> {
        let this = Self {
            os_build: os_build.into(),
            device: device.into(),
            driver: driver.into(),
            reset_generation,
        };
        this.validate()?;
        Ok(this)
    }

    /// Validates string field bounds against [`MAX_IDENTITY_FIELD_LEN`].
    pub fn validate(&self) -> Result<(), CapabilityError> {
        validate_identity_field("os_build", &self.os_build)?;
        validate_identity_field("device", &self.device)?;
        validate_identity_field("driver", &self.driver)?;
        Ok(())
    }
}

/// The measured result of a real probe. Every field is something the probe
/// *observed*, not something a capability bit claimed (plan section 8.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaCapabilities {
    /// Profiles proven decodable/encodable by the probe.
    pub profiles: Vec<CodecProfile>,
    /// Maximum coded width proven.
    pub max_width: u32,
    /// Maximum coded height proven.
    pub max_height: u32,
    /// Surface formats proven to interoperate with the codec.
    pub accepted_formats: Vec<PixelFormat>,
    /// Whether the probe confirmed hardware (not software) acceleration.
    pub hardware_accelerated: bool,
    /// The maximum number of simultaneous codec sessions the probe/device
    /// documents. Admission never exceeds this.
    pub max_sessions: u32,
}

impl MediaCapabilities {
    /// Constructs and validates a new media capabilities record.
    pub fn new(
        profiles: Vec<CodecProfile>,
        max_width: u32,
        max_height: u32,
        accepted_formats: Vec<PixelFormat>,
        hardware_accelerated: bool,
        max_sessions: u32,
    ) -> Result<Self, CapabilityError> {
        let this = Self {
            profiles,
            max_width,
            max_height,
            accepted_formats,
            hardware_accelerated,
            max_sessions,
        };
        this.validate()?;
        Ok(this)
    }

    /// Validates collection bounds and dimensions.
    pub fn validate(&self) -> Result<(), CapabilityError> {
        if self.profiles.len() > MAX_PROBED_PROFILES {
            return Err(CapabilityError::TooManyProfiles {
                count: self.profiles.len(),
                max: MAX_PROBED_PROFILES,
            });
        }
        if self.accepted_formats.len() > MAX_PROBED_FORMATS {
            return Err(CapabilityError::TooManyFormats {
                count: self.accepted_formats.len(),
                max: MAX_PROBED_FORMATS,
            });
        }
        if self.max_width == 0 || self.max_height == 0 {
            return Err(CapabilityError::ZeroDimension);
        }
        Ok(())
    }

    /// True when the probe proved this profile.
    #[must_use]
    pub fn supports_profile(&self, profile: CodecProfile) -> bool {
        self.profiles.contains(&profile)
    }

    /// True when a coded geometry is within the probed maxima.
    #[must_use]
    pub const fn supports_geometry(&self, width: u32, height: u32) -> bool {
        width <= self.max_width && height <= self.max_height
    }
}

/// A probe result bound to the identity it was measured on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedProbe {
    identity: DeviceIdentity,
    capabilities: MediaCapabilities,
}

/// A cache of probe results keyed by device identity. Results are only
/// returned when the requested identity matches exactly (including
/// `reset_generation`); a device loss/reset therefore misses the cache and
/// forces a fresh probe (plan section 8.2).
///
/// Storage is bounded to [`MAX_PROBE_CACHE_ENTRIES`]; overflowing entries evict
/// the oldest probe to prevent unbounded memory growth (AGENTS.md section 5).
#[derive(Debug, Clone, Default)]
pub struct ProbeCache {
    entries: Vec<CachedProbe>,
}

impl ProbeCache {
    /// An empty cache with storage bounded to [`MAX_PROBE_CACHE_ENTRIES`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Records a fresh probe result, replacing any prior entry for a device
    /// with the same OS/device/driver (across reset generations). If the cache
    /// is at capacity, the oldest entry is evicted to stay strictly bounded.
    pub fn insert(
        &mut self,
        identity: DeviceIdentity,
        capabilities: MediaCapabilities,
    ) -> Result<(), CapabilityError> {
        identity.validate()?;
        capabilities.validate()?;
        self.entries.retain(|e| {
            !(e.identity.os_build == identity.os_build
                && e.identity.device == identity.device
                && e.identity.driver == identity.driver)
        });
        if self.entries.len() >= MAX_PROBE_CACHE_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(CachedProbe {
            identity,
            capabilities,
        });
        Ok(())
    }

    /// Returns a cached result only when the identity matches exactly. A
    /// mismatch (including a bumped `reset_generation`) returns `None`,
    /// forcing a re-probe.
    #[must_use]
    pub fn get(&self, identity: &DeviceIdentity) -> Option<&MediaCapabilities> {
        self.entries
            .iter()
            .find(|e| &e.identity == identity)
            .map(|e| &e.capabilities)
    }

    /// Explicitly invalidates every result for a device across reset
    /// generations (e.g. on an observed device-lost event before the new
    /// identity is known).
    pub fn invalidate_device(&mut self, os_build: &str, device: &str, driver: &str) {
        self.entries.retain(|e| {
            !(e.identity.os_build == os_build
                && e.identity.device == device
                && e.identity.driver == driver)
        });
    }

    /// Number of cached entries (for diagnostics/tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Detailed, sanitized report of a completed capability probe (plan section 8.2).
///
/// Records device identity, effective capability limits, and copy ledger
/// counts for diagnostics without exposing screen contents or credentials
/// (AGENTS.md sections 6, 7; plan section 8.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// Target device identity.
    pub identity: DeviceIdentity,
    /// Probed capability bounds.
    pub capabilities: MediaCapabilities,
    /// Observed copy counts during probe execution.
    pub copy_counts: CopyLedger,
}

impl ProbeReport {
    /// Creates a new probe report.
    #[must_use]
    pub fn new(
        identity: DeviceIdentity,
        capabilities: MediaCapabilities,
        copy_counts: CopyLedger,
    ) -> Self {
        Self {
            identity,
            capabilities,
            copy_counts,
        }
    }

    /// Formats a sanitized single-line diagnostic summary for logging.
    #[must_use]
    pub fn diagnostic_summary(&self) -> String {
        format!(
            "probe[device=\"{}\" driver=\"{}\" os=\"{}\" gen={} hw={} max_res={}x{} max_sess={} profiles={} formats={} copies={}]",
            self.identity.device,
            self.identity.driver,
            self.identity.os_build,
            self.identity.reset_generation,
            self.capabilities.hardware_accelerated,
            self.capabilities.max_width,
            self.capabilities.max_height,
            self.capabilities.max_sessions,
            self.capabilities.profiles.len(),
            self.capabilities.accepted_formats.len(),
            self.copy_counts.total()
        )
    }
}

/// Why a session could not be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionError {
    /// The device's simultaneous-session capacity is exhausted.
    SessionCapacityExhausted {
        /// The capacity ceiling.
        max_sessions: u32,
    },
}

/// Admits live encoder/decoder sessions against the probed simultaneous-session
/// capacity. A probe proving a device *can* encode is not permission for an
/// unbounded number of concurrent sessions; each session is admitted and
/// released explicitly (plan sections 8.2, 19.3).
#[derive(Debug, Clone)]
pub struct SessionAdmission {
    max_sessions: u32,
    active: u32,
}

impl SessionAdmission {
    /// Creates an admission controller for a device with `max_sessions`
    /// simultaneous codec sessions.
    #[must_use]
    pub const fn new(max_sessions: u32) -> Self {
        Self {
            max_sessions,
            active: 0,
        }
    }

    /// Admits one session, or refuses when capacity is exhausted. The returned
    /// count is the number now active.
    pub fn admit(&mut self) -> Result<u32, AdmissionError> {
        if self.active >= self.max_sessions {
            return Err(AdmissionError::SessionCapacityExhausted {
                max_sessions: self.max_sessions,
            });
        }
        self.active += 1;
        Ok(self.active)
    }

    /// Releases one session. Saturating: releasing more than were admitted is
    /// a no-op at zero rather than an underflow.
    pub fn release(&mut self) {
        self.active = self.active.saturating_sub(1);
    }

    /// Currently active sessions.
    #[must_use]
    pub const fn active(&self) -> u32 {
        self.active
    }

    /// Whether another session could be admitted right now.
    #[must_use]
    pub const fn has_capacity(&self) -> bool {
        self.active < self.max_sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(reset: u64) -> DeviceIdentity {
        DeviceIdentity::new("macOS 15.0 (24A335)", "Apple M3 Max", "vt-1.2.3", reset).unwrap()
    }

    fn caps() -> MediaCapabilities {
        MediaCapabilities::new(
            vec![CodecProfile::Main8_420],
            3840,
            2160,
            vec![PixelFormat::Nv12],
            true,
            2,
        )
        .unwrap()
    }

    #[test]
    fn probe_cache_hits_exact_identity_and_misses_after_reset() {
        let mut cache = ProbeCache::new();
        assert!(cache.is_empty());
        cache.insert(identity(0), caps()).unwrap();
        assert_eq!(cache.len(), 1);
        // Exact match hits.
        assert!(cache.get(&identity(0)).is_some());
        // A device reset (bumped generation) misses -> forces re-probe.
        assert!(cache.get(&identity(1)).is_none());
        // Re-probing replaces the stale entry rather than accumulating.
        cache.insert(identity(1), caps()).unwrap();
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&identity(1)).is_some());
    }

    #[test]
    fn probe_cache_explicit_invalidation_clears_the_device() {
        let mut cache = ProbeCache::new();
        cache.insert(identity(5), caps()).unwrap();
        cache.invalidate_device("macOS 15.0 (24A335)", "Apple M3 Max", "vt-1.2.3");
        assert!(cache.is_empty());
        assert!(cache.get(&identity(5)).is_none());
    }

    #[test]
    fn probe_cache_bounds_entries_and_evicts_oldest() {
        let mut cache = ProbeCache::new();
        for i in 0..20 {
            let id =
                DeviceIdentity::new("Linux 6.8.0", format!("GPU-{i:02}"), "nvidia-550", 0).unwrap();
            cache.insert(id, caps()).unwrap();
        }
        assert_eq!(cache.len(), MAX_PROBE_CACHE_ENTRIES);
        assert_eq!(cache.len(), 16);

        // The first 4 GPUs (0..4) should have been evicted.
        for i in 0..4 {
            let old_id =
                DeviceIdentity::new("Linux 6.8.0", format!("GPU-{i:02}"), "nvidia-550", 0).unwrap();
            assert!(cache.get(&old_id).is_none());
        }

        // The remaining 16 GPUs (4..20) should still be in cache.
        for i in 4..20 {
            let id =
                DeviceIdentity::new("Linux 6.8.0", format!("GPU-{i:02}"), "nvidia-550", 0).unwrap();
            assert!(cache.get(&id).is_some());
        }
    }

    #[test]
    fn identity_rejects_oversized_and_empty_fields() {
        assert_eq!(
            DeviceIdentity::new("", "GPU", "driver", 0),
            Err(CapabilityError::IdentityFieldEmpty { field: "os_build" })
        );
        let oversized = "x".repeat(MAX_IDENTITY_FIELD_LEN + 1);
        assert_eq!(
            DeviceIdentity::new("OS", &oversized, "driver", 0),
            Err(CapabilityError::IdentityFieldTooLong {
                field: "device",
                len: MAX_IDENTITY_FIELD_LEN + 1,
                max: MAX_IDENTITY_FIELD_LEN,
            })
        );
    }

    #[test]
    fn capabilities_reject_oversized_collections_and_zero_dims() {
        let too_many_profiles = vec![CodecProfile::Main8_420; MAX_PROBED_PROFILES + 1];
        assert_eq!(
            MediaCapabilities::new(
                too_many_profiles,
                1920,
                1080,
                vec![PixelFormat::Nv12],
                true,
                1
            ),
            Err(CapabilityError::TooManyProfiles {
                count: MAX_PROBED_PROFILES + 1,
                max: MAX_PROBED_PROFILES,
            })
        );

        let too_many_formats = vec![PixelFormat::Nv12; MAX_PROBED_FORMATS + 1];
        assert_eq!(
            MediaCapabilities::new(
                vec![CodecProfile::Main8_420],
                1920,
                1080,
                too_many_formats,
                true,
                1
            ),
            Err(CapabilityError::TooManyFormats {
                count: MAX_PROBED_FORMATS + 1,
                max: MAX_PROBED_FORMATS,
            })
        );

        assert_eq!(
            MediaCapabilities::new(
                vec![CodecProfile::Main8_420],
                0,
                1080,
                vec![PixelFormat::Nv12],
                true,
                1
            ),
            Err(CapabilityError::ZeroDimension)
        );
    }

    #[test]
    fn capabilities_report_probed_support() {
        let c = caps();
        assert!(c.supports_profile(CodecProfile::Main8_420));
        assert!(c.supports_geometry(3840, 2160));
        assert!(!c.supports_geometry(3841, 2160));
    }

    #[test]
    fn probe_report_diagnostic_summary_is_sanitized() {
        let mut ledger = CopyLedger::new();
        ledger.record(crate::surface::CopyKind::GpuConversion);
        ledger.record(crate::surface::CopyKind::CaptureToOwned);

        let report = ProbeReport::new(identity(1), caps(), ledger);
        let summary = report.diagnostic_summary();
        assert!(summary.contains("device=\"Apple M3 Max\""));
        assert!(summary.contains("hw=true"));
        assert!(summary.contains("max_res=3840x2160"));
        assert!(summary.contains("copies=2"));
    }

    #[test]
    fn session_admission_bounds_concurrency() {
        let mut adm = SessionAdmission::new(2);
        assert_eq!(adm.admit(), Ok(1));
        assert_eq!(adm.admit(), Ok(2));
        assert!(!adm.has_capacity());
        assert_eq!(
            adm.admit(),
            Err(AdmissionError::SessionCapacityExhausted { max_sessions: 2 })
        );
        adm.release();
        assert!(adm.has_capacity());
        assert_eq!(adm.admit(), Ok(2));
        // Over-release saturates at zero.
        adm.release();
        adm.release();
        adm.release();
        assert_eq!(adm.active(), 0);
    }
}
