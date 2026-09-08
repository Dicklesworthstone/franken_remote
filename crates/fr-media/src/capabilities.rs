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

use crate::config::CodecProfile;
use crate::surface::PixelFormat;

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
#[derive(Debug, Clone, Default)]
pub struct ProbeCache {
    entries: Vec<CachedProbe>,
}

impl ProbeCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Records a fresh probe result, replacing any prior entry for a device
    /// with the same OS/device/driver (across reset generations).
    pub fn insert(&mut self, identity: DeviceIdentity, capabilities: MediaCapabilities) {
        self.entries.retain(|e| {
            !(e.identity.os_build == identity.os_build
                && e.identity.device == identity.device
                && e.identity.driver == identity.driver)
        });
        self.entries.push(CachedProbe { identity, capabilities });
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
        Self { max_sessions, active: 0 }
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
        DeviceIdentity {
            os_build: "macOS 15.0 (24A335)".to_string(),
            device: "Apple M3 Max".to_string(),
            driver: "vt-1.2.3".to_string(),
            reset_generation: reset,
        }
    }

    fn caps() -> MediaCapabilities {
        MediaCapabilities {
            profiles: vec![CodecProfile::Main8_420],
            max_width: 3840,
            max_height: 2160,
            accepted_formats: vec![PixelFormat::Nv12],
            hardware_accelerated: true,
            max_sessions: 2,
        }
    }

    #[test]
    fn probe_cache_hits_exact_identity_and_misses_after_reset() {
        let mut cache = ProbeCache::new();
        assert!(cache.is_empty());
        cache.insert(identity(0), caps());
        assert_eq!(cache.len(), 1);
        // Exact match hits.
        assert!(cache.get(&identity(0)).is_some());
        // A device reset (bumped generation) misses -> forces re-probe.
        assert!(cache.get(&identity(1)).is_none());
        // Re-probing replaces the stale entry rather than accumulating.
        cache.insert(identity(1), caps());
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&identity(1)).is_some());
    }

    #[test]
    fn probe_cache_explicit_invalidation_clears_the_device() {
        let mut cache = ProbeCache::new();
        cache.insert(identity(5), caps());
        cache.invalidate_device("macOS 15.0 (24A335)", "Apple M3 Max", "vt-1.2.3");
        assert!(cache.is_empty());
        assert!(cache.get(&identity(5)).is_none());
    }

    #[test]
    fn capabilities_report_probed_support() {
        let c = caps();
        assert!(c.supports_profile(CodecProfile::Main8_420));
        assert!(c.supports_geometry(3840, 2160));
        assert!(!c.supports_geometry(3841, 2160));
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
