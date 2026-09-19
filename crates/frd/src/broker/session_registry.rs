//! Session registry tracking admitted viewers, controller, and generation fencing (plan sections 4, 5.1, 5.2, 7.2, 19.2).
//!
//! Enforces:
//! - Separate state variables for observation authority, media readiness, and input authority.
//! - At most ONE active controller session (input lease holder) at any time.
//! - Bounded viewer capacity.
//! - Generation fencing: requests tagged with stale generations are rejected.
//! - Orderly teardown: revoke input -> invalidate generations -> close sessions.

use super::process_role::ProcessGeneration;
use core::fmt;
use fr_core::ids::{
    CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputLeaseId, OsSessionId,
    RemoteSessionId,
};
use std::net::IpAddr;

/// Pipeline media readiness state for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaReadinessState {
    /// Not started yet.
    NotStarted,
    /// Decoder/encoder configuration in progress.
    Configuring,
    /// Actively receiving or streaming media frames.
    Active,
    /// Temporary pipeline stall (e.g. loss recovery).
    Stalled,
    /// Pipeline stopped or closed.
    Stopped,
}

/// An admitted remote session record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// Unique remote session identity.
    pub session_id: RemoteSessionId,
    /// Client tailnet IP.
    pub peer_ip: IpAddr,
    /// Authenticated user login.
    pub peer_login: String,
    /// Monotonic timestamp (us) when session was admitted.
    pub admitted_at_us: u64,
    /// Observation authority granted by user consent.
    pub has_observation_authority: bool,
    /// Current media pipeline readiness.
    pub media_readiness: MediaReadinessState,
    /// Active input lease ID if this session is the active controller.
    pub active_input_lease: Option<InputLeaseId>,
    /// Expiration timestamp (us) of current input lease, if any.
    pub lease_expires_at_us: Option<u64>,
    /// Last received heartbeat timestamp (us).
    pub last_heartbeat_us: u64,
}

impl SessionRecord {
    /// True if this session holds active, unexpired input authority.
    #[must_use]
    pub fn is_active_controller(&self, now_us: u64) -> bool {
        self.active_input_lease.is_some()
            && self
                .lease_expires_at_us
                .is_some_and(|expires| now_us < expires)
    }
}

/// Master session registry for `frd`.
#[derive(Debug)]
pub struct SessionRegistry {
    /// Host boot identity.
    pub host_boot_id: HostBootId,
    /// Current interactive OS session identity.
    pub os_session_id: OsSessionId,
    /// Active process generation.
    pub process_generation: ProcessGeneration,
    /// Active display geometry generation.
    pub geometry_generation: DisplayGeometryGeneration,
    /// Active codec configuration generation.
    pub codec_generation: CodecConfigurationGeneration,
    /// Maximum concurrent viewer sessions permitted.
    pub max_viewers: usize,
    /// Admitted sessions.
    sessions: Vec<SessionRecord>,
    /// Currently assigned controller session ID (if any).
    active_controller_id: Option<RemoteSessionId>,
}

/// Typed errors in session registry operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// Registry has reached maximum viewer capacity.
    CapacityExceeded { current: usize, maximum: usize },
    /// Another session already holds active, unexpired input authority.
    ControllerAlreadyActive { active_session_id: RemoteSessionId },
    /// Session was not found in registry.
    SessionNotFound,
    /// Session is not the active controller.
    NotController,
    /// Operation attempted with a stale generation.
    StaleGeneration { expected: u64, actual: u64 },
    /// Input lease has already expired.
    ExpiredLease,
    /// Session does not have observation authority.
    ObservationAuthorityMissing,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExceeded { current, maximum } => {
                write!(f, "viewer capacity exceeded ({current} >= {maximum})")
            }
            Self::ControllerAlreadyActive { active_session_id } => {
                write!(
                    f,
                    "another session ({active_session_id}) already holds input authority"
                )
            }
            Self::SessionNotFound => f.write_str("session not found in registry"),
            Self::NotController => f.write_str("session is not the active controller"),
            Self::StaleGeneration { expected, actual } => {
                write!(f, "stale generation: active {expected}, got {actual}")
            }
            Self::ExpiredLease => f.write_str("input lease has expired"),
            Self::ObservationAuthorityMissing => f.write_str("session lacks observation authority"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Result of an orderly teardown operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeardownPlan {
    /// Number of sessions closed.
    pub sessions_closed: usize,
    /// Whether an active input controller was revoked.
    pub controller_revoked: bool,
    /// New successor process generation.
    pub next_generation: ProcessGeneration,
}

impl SessionRegistry {
    /// Initialize a new session registry.
    #[must_use]
    pub fn new(host_boot_id: HostBootId, os_session_id: OsSessionId, max_viewers: usize) -> Self {
        Self {
            host_boot_id,
            os_session_id,
            process_generation: ProcessGeneration::INITIAL,
            geometry_generation: DisplayGeometryGeneration::INITIAL,
            codec_generation: CodecConfigurationGeneration::INITIAL,
            max_viewers: max_viewers.max(1),
            sessions: Vec::with_capacity(max_viewers.min(16)),
            active_controller_id: None,
        }
    }

    /// Admit a new viewer session after tailnet authentication and user consent.
    pub fn admit_viewer(
        &mut self,
        session_id: RemoteSessionId,
        peer_ip: IpAddr,
        peer_login: String,
        has_observation: bool,
        now_us: u64,
    ) -> Result<(), RegistryError> {
        if self.sessions.len() >= self.max_viewers {
            return Err(RegistryError::CapacityExceeded {
                current: self.sessions.len(),
                maximum: self.max_viewers,
            });
        }
        if self.sessions.iter().any(|s| s.session_id == session_id) {
            return Ok(()); // already admitted
        }

        self.sessions.push(SessionRecord {
            session_id,
            peer_ip,
            peer_login,
            admitted_at_us: now_us,
            has_observation_authority: has_observation,
            media_readiness: MediaReadinessState::NotStarted,
            active_input_lease: None,
            lease_expires_at_us: None,
            last_heartbeat_us: now_us,
        });

        Ok(())
    }

    /// Promote an admitted viewer session to active input controller.
    pub fn grant_input_lease(
        &mut self,
        session_id: RemoteSessionId,
        lease_id: InputLeaseId,
        lease_duration_us: u64,
        now_us: u64,
    ) -> Result<(), RegistryError> {
        // Check if there is an existing active controller.
        if let Some(current_controller) = self.active_controller_id
            && current_controller != session_id
            && let Some(rec) = self
                .sessions
                .iter()
                .find(|s| s.session_id == current_controller)
            && rec.is_active_controller(now_us)
        {
            return Err(RegistryError::ControllerAlreadyActive {
                active_session_id: current_controller,
            });
        }

        let session = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
            .ok_or(RegistryError::SessionNotFound)?;

        if !session.has_observation_authority {
            return Err(RegistryError::ObservationAuthorityMissing);
        }

        let expires_at_us = now_us.saturating_add(lease_duration_us);
        session.active_input_lease = Some(lease_id);
        session.lease_expires_at_us = Some(expires_at_us);
        self.active_controller_id = Some(session_id);

        Ok(())
    }

    /// Revoke active input authority from the current controller.
    pub fn revoke_input_lease(&mut self, session_id: RemoteSessionId) -> Result<(), RegistryError> {
        let session = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
            .ok_or(RegistryError::SessionNotFound)?;

        session.active_input_lease = None;
        session.lease_expires_at_us = None;

        if self.active_controller_id == Some(session_id) {
            self.active_controller_id = None;
        }

        Ok(())
    }

    /// Update media pipeline readiness state.
    pub fn set_media_readiness(
        &mut self,
        session_id: RemoteSessionId,
        readiness: MediaReadinessState,
    ) -> Result<(), RegistryError> {
        let session = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
            .ok_or(RegistryError::SessionNotFound)?;

        session.media_readiness = readiness;
        Ok(())
    }

    /// Record a heartbeat timestamp for an active session.
    pub fn record_heartbeat(
        &mut self,
        session_id: RemoteSessionId,
        now_us: u64,
    ) -> Result<(), RegistryError> {
        let session = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
            .ok_or(RegistryError::SessionNotFound)?;

        session.last_heartbeat_us = now_us;
        Ok(())
    }

    /// Validate an input submission action against active controller, lease, and generation.
    pub fn validate_input_submission(
        &self,
        session_id: RemoteSessionId,
        lease_id: InputLeaseId,
        geometry_gen: DisplayGeometryGeneration,
        now_us: u64,
    ) -> Result<(), RegistryError> {
        if self.active_controller_id != Some(session_id) {
            return Err(RegistryError::NotController);
        }

        let session = self
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .ok_or(RegistryError::SessionNotFound)?;

        if session.active_input_lease != Some(lease_id) {
            return Err(RegistryError::NotController);
        }

        if !session.is_active_controller(now_us) {
            return Err(RegistryError::ExpiredLease);
        }

        if geometry_gen != self.geometry_generation {
            return Err(RegistryError::StaleGeneration {
                expected: self.geometry_generation.as_raw(),
                actual: geometry_gen.as_raw(),
            });
        }

        Ok(())
    }

    /// Advance display geometry generation (fences all subsequent input to new layout).
    pub fn advance_geometry_generation(&mut self) -> Option<DisplayGeometryGeneration> {
        let next = self.geometry_generation.next()?;
        self.geometry_generation = next;
        Some(next)
    }

    /// Advance codec configuration generation.
    pub fn advance_codec_generation(&mut self) -> Option<CodecConfigurationGeneration> {
        let next = self.codec_generation.next()?;
        self.codec_generation = next;
        Some(next)
    }

    /// Advance process generation.
    pub fn advance_process_generation(&mut self) -> Option<ProcessGeneration> {
        let next = self.process_generation.next()?;
        self.process_generation = next;
        Some(next)
    }

    /// Close and remove a session.
    pub fn remove_session(&mut self, session_id: RemoteSessionId) -> bool {
        if self.active_controller_id == Some(session_id) {
            self.active_controller_id = None;
        }
        if let Some(pos) = self
            .sessions
            .iter()
            .position(|s| s.session_id == session_id)
        {
            self.sessions.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Perform full orderly teardown: revoke input -> invalidate generations -> close sessions.
    pub fn teardown_all(&mut self) -> TeardownPlan {
        let count = self.sessions.len();
        let had_controller = self.active_controller_id.is_some();

        // 1. Invalidate controller
        self.active_controller_id = None;
        for s in &mut self.sessions {
            s.active_input_lease = None;
            s.lease_expires_at_us = None;
            s.media_readiness = MediaReadinessState::Stopped;
        }

        // 2. Advance generations to fence any in-flight work
        let next_gen = self
            .process_generation
            .next()
            .unwrap_or(self.process_generation);
        self.process_generation = next_gen;
        if let Some(g) = self.geometry_generation.next() {
            self.geometry_generation = g;
        }
        if let Some(c) = self.codec_generation.next() {
            self.codec_generation = c;
        }

        // 3. Clear sessions
        self.sessions.clear();

        TeardownPlan {
            sessions_closed: count,
            controller_revoked: had_controller,
            next_generation: next_gen,
        }
    }

    /// Total active admitted sessions.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Currently active controller session ID, if any.
    #[must_use]
    pub fn active_controller(&self) -> Option<RemoteSessionId> {
        self.active_controller_id
    }

    /// Read-only slice of admitted session records.
    #[must_use]
    pub fn sessions(&self) -> &[SessionRecord] {
        &self.sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn make_registry() -> SessionRegistry {
        SessionRegistry::new(
            HostBootId::from_raw(1),
            OsSessionId::from_raw(2),
            3, // max 3 viewers
        )
    }

    #[test]
    fn admit_viewers_up_to_capacity() {
        let mut reg = make_registry();
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));

        let s1 = RemoteSessionId::from_raw(101);
        let s2 = RemoteSessionId::from_raw(102);
        let s3 = RemoteSessionId::from_raw(103);
        let s4 = RemoteSessionId::from_raw(104);

        assert!(
            reg.admit_viewer(s1, ip, "a@b.com".into(), true, 1000)
                .is_ok()
        );
        assert!(
            reg.admit_viewer(s2, ip, "a@b.com".into(), true, 1000)
                .is_ok()
        );
        assert!(
            reg.admit_viewer(s3, ip, "a@b.com".into(), true, 1000)
                .is_ok()
        );
        assert_eq!(reg.session_count(), 3);

        // 4th viewer exceeds capacity.
        let err = reg.admit_viewer(s4, ip, "a@b.com".into(), true, 1000);
        assert_eq!(
            err,
            Err(RegistryError::CapacityExceeded {
                current: 3,
                maximum: 3,
            })
        );
    }

    #[test]
    fn single_controller_invariant_and_conflict_rejection() {
        let mut reg = make_registry();
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        let s1 = RemoteSessionId::from_raw(101);
        let s2 = RemoteSessionId::from_raw(102);

        reg.admit_viewer(s1, ip, "a@b.com".into(), true, 1000)
            .unwrap();
        reg.admit_viewer(s2, ip, "a@b.com".into(), true, 1000)
            .unwrap();

        let l1 = InputLeaseId::from_raw(501);
        let l2 = InputLeaseId::from_raw(502);

        // s1 acquires input lease for 3 seconds (expires at t=4000).
        reg.grant_input_lease(s1, l1, 3000, 1000).unwrap();
        assert_eq!(reg.active_controller(), Some(s1));

        // s2 attempts to acquire input lease while s1 is active -> refused!
        let res = reg.grant_input_lease(s2, l2, 3000, 2000);
        assert_eq!(
            res,
            Err(RegistryError::ControllerAlreadyActive {
                active_session_id: s1,
            })
        );

        // After s1's lease expires at t=4001, s2 can acquire input lease.
        reg.grant_input_lease(s2, l2, 3000, 4001).unwrap();
        assert_eq!(reg.active_controller(), Some(s2));
    }

    #[test]
    fn input_submission_validation_enforces_active_lease_and_generation() {
        let mut reg = make_registry();
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        let s1 = RemoteSessionId::from_raw(101);
        reg.admit_viewer(s1, ip, "a@b.com".into(), true, 1000)
            .unwrap();

        let l1 = InputLeaseId::from_raw(501);
        reg.grant_input_lease(s1, l1, 3000, 1000).unwrap();

        let geo = DisplayGeometryGeneration::INITIAL;
        assert!(reg.validate_input_submission(s1, l1, geo, 2000).is_ok());

        // Submission with expired lease is refused.
        assert_eq!(
            reg.validate_input_submission(s1, l1, geo, 4500),
            Err(RegistryError::ExpiredLease)
        );

        // Advancing geometry invalidates previous generation submissions.
        reg.advance_geometry_generation().unwrap();
        assert!(matches!(
            reg.validate_input_submission(s1, l1, geo, 2500),
            Err(RegistryError::StaleGeneration { .. })
        ));
    }

    #[test]
    fn teardown_cleans_up_and_fences_generations() {
        let mut reg = make_registry();
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        let s1 = RemoteSessionId::from_raw(101);
        reg.admit_viewer(s1, ip, "a@b.com".into(), true, 1000)
            .unwrap();
        reg.grant_input_lease(s1, InputLeaseId::from_raw(501), 3000, 1000)
            .unwrap();

        let initial_gen = reg.process_generation;
        let plan = reg.teardown_all();

        assert_eq!(plan.sessions_closed, 1);
        assert!(plan.controller_revoked);
        assert!(plan.next_generation.supersedes(initial_gen));
        assert_eq!(reg.session_count(), 0);
        assert_eq!(reg.active_controller(), None);
    }
}
