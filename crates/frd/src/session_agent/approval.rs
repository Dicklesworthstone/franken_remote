//! Local approval UI gating and consent state machine (plan §§5.3, 6.1, 15.2).
//!
//! Gates any new observation (pixels, thumbnails, audio, clipboard, semantic data)
//! before local prompt and approval. Mode changes revoke existing grants.
//! Approval records device identity, role, displays, audio scope, and session ID.

use fr_core::{ids::RemoteSessionId, time::HostInstant};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Maximum duration an unapproved request may remain pending before timing out.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Interactive session approval policy mode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Interactive desktop prompt required for every session (default).
    #[default]
    PromptAlways,
    /// Pre-authorized local approvals, prompt otherwise.
    ExplicitLocal,
    /// Unattended access (must be explicitly enabled by administrator).
    Unattended,
}

/// Client role requested for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    /// Read-only observer (screen, audio playback).
    Observer,
    /// Full controller (input lease, clipboard, files).
    Controller,
}

/// Audio direction and capabilities requested/granted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum AudioScope {
    #[default]
    None,
    PlaybackOnly,
    MicrophoneOnly,
    Bidirectional,
}

impl AudioScope {
    /// Returns true if `self` is a subset of or equal to `allowed`.
    pub fn is_subset_of(&self, allowed: AudioScope) -> bool {
        matches!(
            (self, allowed),
            (AudioScope::None, _)
                | (
                    AudioScope::PlaybackOnly,
                    AudioScope::PlaybackOnly | AudioScope::Bidirectional
                )
                | (
                    AudioScope::MicrophoneOnly,
                    AudioScope::MicrophoneOnly | AudioScope::Bidirectional
                )
                | (AudioScope::Bidirectional, AudioScope::Bidirectional)
        )
    }
}

/// Verified Tailscale peer identity metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    pub node_id: String,
    pub node_name: String,
    pub user_id: String,
}

/// Scope of capabilities requested by a connecting peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedScope {
    pub role: SessionRole,
    pub displays: Vec<u32>,
    pub audio: AudioScope,
    pub clipboard: bool,
    pub file_transfer: bool,
}

/// Scope of capabilities granted by the local user or policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedScope {
    pub role: SessionRole,
    pub displays: Vec<u32>,
    pub audio: AudioScope,
    pub clipboard: bool,
    pub file_transfer: bool,
    pub granted_at: HostInstant,
    pub expires_at: Option<HostInstant>,
}

impl GrantedScope {
    /// Verify that this grant does not exceed what was requested.
    pub fn is_clamped_subset_of(&self, requested: &RequestedScope) -> bool {
        // Role cannot be escalated from Observer to Controller
        if self.role == SessionRole::Controller && requested.role == SessionRole::Observer {
            return false;
        }
        // Observer role never receives microphone authority (Plan §15.4)
        if self.role == SessionRole::Observer
            && matches!(
                self.audio,
                AudioScope::MicrophoneOnly | AudioScope::Bidirectional
            )
        {
            return false;
        }
        // Audio must be a subset of requested audio
        if !self.audio.is_subset_of(requested.audio) {
            return false;
        }
        // Clipboard and file transfer cannot be enabled if not requested
        if self.clipboard && !requested.clipboard {
            return false;
        }
        if self.file_transfer && !requested.file_transfer {
            return false;
        }
        // Displays must only contain displays requested
        for d in &self.displays {
            if !requested.displays.contains(d) {
                return false;
            }
        }
        true
    }
}

/// Reason for refusing an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialReason {
    UserRejected,
    TimedOut,
    SessionEnded,
    PolicyForbidden,
    ModeChanged,
    AlreadyExists,
}

/// Approval state of a session request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalState {
    /// Gated before prompt. No observation (pixels, audio, clipboard) is permitted.
    Pending {
        requested_at: HostInstant,
        expires_at: HostInstant,
    },
    /// Local user or policy granted the specified scope.
    Approved(GrantedScope),
    /// Request was denied.
    Denied(DenialReason),
    /// Request or session was revoked.
    Revoked,
}

/// Record of an approval request for auditing and UI inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRecord {
    pub session_id: RemoteSessionId,
    pub peer: PeerIdentity,
    pub requested: RequestedScope,
    pub state: ApprovalState,
}

/// Manages local user approval and observation gating.
pub struct ApprovalManager {
    mode: ApprovalMode,
    requests: HashMap<RemoteSessionId, ApprovalRecord>,
    pre_authorized_nodes: HashSet<String>,
}

impl ApprovalManager {
    pub fn new(mode: ApprovalMode) -> Self {
        Self {
            mode,
            requests: HashMap::new(),
            pre_authorized_nodes: HashSet::new(),
        }
    }

    pub fn mode(&self) -> ApprovalMode {
        self.mode
    }

    /// Change approval mode. Mode transitions immediately revoke existing grants
    /// to prevent stale or policy-violating authority inheritance.
    pub fn set_mode(&mut self, new_mode: ApprovalMode) {
        if self.mode != new_mode {
            self.mode = new_mode;
            // Revoke all existing approved grants on mode change
            for record in self.requests.values_mut() {
                if matches!(record.state, ApprovalState::Approved(_)) {
                    record.state = ApprovalState::Revoked;
                } else if matches!(record.state, ApprovalState::Pending { .. }) {
                    record.state = ApprovalState::Denied(DenialReason::ModeChanged);
                }
            }
        }
    }

    /// Add a pre-authorized node ID for `ApprovalMode::ExplicitLocal`.
    pub fn add_pre_authorized_node(&mut self, node_id: impl Into<String>) {
        self.pre_authorized_nodes.insert(node_id.into());
    }

    /// Remove a pre-authorized node ID.
    pub fn remove_pre_authorized_node(&mut self, node_id: &str) {
        self.pre_authorized_nodes.remove(node_id);
    }

    /// Submit a new connection request. Returns the resulting approval state.
    /// In all prompting modes, any new observation is strictly gated until approved.
    pub fn request_approval(
        &mut self,
        session_id: RemoteSessionId,
        peer: PeerIdentity,
        requested: RequestedScope,
        now: HostInstant,
    ) -> Result<ApprovalState, DenialReason> {
        if self.requests.contains_key(&session_id) {
            return Err(DenialReason::AlreadyExists);
        }

        let state = match self.mode {
            ApprovalMode::Unattended => {
                // Auto-approve requested scope in unattended mode
                // Note: Observer role never receives microphone authority (Plan §15.4)
                let granted_audio = if requested.role == SessionRole::Observer {
                    match requested.audio {
                        AudioScope::Bidirectional | AudioScope::PlaybackOnly => {
                            AudioScope::PlaybackOnly
                        }
                        AudioScope::MicrophoneOnly | AudioScope::None => AudioScope::None,
                    }
                } else {
                    requested.audio
                };
                let grant = GrantedScope {
                    role: requested.role,
                    displays: requested.displays.clone(),
                    audio: granted_audio,
                    clipboard: requested.clipboard,
                    file_transfer: requested.file_transfer,
                    granted_at: now,
                    expires_at: None,
                };
                ApprovalState::Approved(grant)
            }
            ApprovalMode::ExplicitLocal => {
                if self.pre_authorized_nodes.contains(&peer.node_id) {
                    let granted_audio = if requested.role == SessionRole::Observer {
                        match requested.audio {
                            AudioScope::Bidirectional | AudioScope::PlaybackOnly => {
                                AudioScope::PlaybackOnly
                            }
                            AudioScope::MicrophoneOnly | AudioScope::None => AudioScope::None,
                        }
                    } else {
                        requested.audio
                    };
                    let grant = GrantedScope {
                        role: requested.role,
                        displays: requested.displays.clone(),
                        audio: granted_audio,
                        clipboard: requested.clipboard,
                        file_transfer: requested.file_transfer,
                        granted_at: now,
                        expires_at: None,
                    };
                    ApprovalState::Approved(grant)
                } else {
                    let timeout_micros =
                        u64::try_from(APPROVAL_TIMEOUT.as_micros()).unwrap_or(u64::MAX);
                    let deadline = now
                        .checked_add(fr_core::time::HostDuration::from_micros(timeout_micros))
                        .unwrap_or(HostInstant::from_micros(u64::MAX));
                    ApprovalState::Pending {
                        requested_at: now,
                        expires_at: deadline,
                    }
                }
            }
            ApprovalMode::PromptAlways => {
                let timeout_micros =
                    u64::try_from(APPROVAL_TIMEOUT.as_micros()).unwrap_or(u64::MAX);
                let deadline = now
                    .checked_add(fr_core::time::HostDuration::from_micros(timeout_micros))
                    .unwrap_or(HostInstant::from_micros(u64::MAX));
                ApprovalState::Pending {
                    requested_at: now,
                    expires_at: deadline,
                }
            }
        };

        let record = ApprovalRecord {
            session_id,
            peer,
            requested,
            state: state.clone(),
        };
        self.requests.insert(session_id, record);
        Ok(state)
    }

    /// Approve a pending request with an explicitly granted scope.
    /// The granted scope is validated and clamped so it never exceeds requested bounds.
    pub fn approve(
        &mut self,
        session_id: RemoteSessionId,
        granted: GrantedScope,
        now: HostInstant,
    ) -> Result<GrantedScope, DenialReason> {
        let record = self
            .requests
            .get_mut(&session_id)
            .ok_or(DenialReason::SessionEnded)?;

        match record.state {
            ApprovalState::Pending { expires_at, .. } => {
                if now >= expires_at {
                    record.state = ApprovalState::Denied(DenialReason::TimedOut);
                    return Err(DenialReason::TimedOut);
                }
                if !granted.is_clamped_subset_of(&record.requested) {
                    return Err(DenialReason::PolicyForbidden);
                }
                record.state = ApprovalState::Approved(granted.clone());
                Ok(granted)
            }
            ApprovalState::Approved(_) => Err(DenialReason::AlreadyExists),
            ApprovalState::Denied(r) => Err(r),
            ApprovalState::Revoked => Err(DenialReason::SessionEnded),
        }
    }

    /// Deny a pending request.
    pub fn deny(
        &mut self,
        session_id: RemoteSessionId,
        reason: DenialReason,
    ) -> Result<(), DenialReason> {
        let record = self
            .requests
            .get_mut(&session_id)
            .ok_or(DenialReason::SessionEnded)?;
        record.state = ApprovalState::Denied(reason);
        Ok(())
    }

    /// Revoke an approved session grant immediately.
    pub fn revoke(&mut self, session_id: RemoteSessionId) -> bool {
        if let Some(record) = self.requests.get_mut(&session_id) {
            record.state = ApprovalState::Revoked;
            true
        } else {
            false
        }
    }

    /// Handle session termination. Expired requesting session ends approval immediately.
    pub fn on_session_ended(&mut self, session_id: RemoteSessionId) {
        if let Some(record) = self.requests.get_mut(&session_id) {
            match record.state {
                ApprovalState::Pending { .. } => {
                    record.state = ApprovalState::Denied(DenialReason::SessionEnded);
                }
                ApprovalState::Approved(_) => {
                    record.state = ApprovalState::Revoked;
                }
                _ => {}
            }
        }
    }

    /// Check if observation (pixels/thumbnails/audio/clipboard) is admitted for this session.
    /// Strict gating: observation is completely blocked while Pending, Denied, or Revoked.
    pub fn is_observation_admitted(&self, session_id: RemoteSessionId) -> bool {
        self.requests
            .get(&session_id)
            .is_some_and(|r| matches!(r.state, ApprovalState::Approved(_)))
    }

    /// Check if control (keyboard/mouse injection) is admitted for this session.
    pub fn is_control_admitted(&self, session_id: RemoteSessionId) -> bool {
        self.requests.get(&session_id).is_some_and(|r| {
            if let ApprovalState::Approved(ref grant) = r.state {
                grant.role == SessionRole::Controller
            } else {
                false
            }
        })
    }

    /// Get the granted scope if approved.
    pub fn granted_scope(&self, session_id: RemoteSessionId) -> Option<&GrantedScope> {
        self.requests.get(&session_id).and_then(|r| match &r.state {
            ApprovalState::Approved(g) => Some(g),
            _ => None,
        })
    }

    /// Inspect the approval record for a session.
    pub fn get_record(&self, session_id: RemoteSessionId) -> Option<&ApprovalRecord> {
        self.requests.get(&session_id)
    }

    /// Clean up expired pending requests.
    pub fn purge_expired(&mut self, now: HostInstant) {
        for record in self.requests.values_mut() {
            if matches!(record.state, ApprovalState::Pending { expires_at, .. } if now >= expires_at)
            {
                record.state = ApprovalState::Denied(DenialReason::TimedOut);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_role_never_gets_microphone_access() {
        let now = HostInstant::from_micros(1000);
        let mut mgr = ApprovalManager::new(ApprovalMode::Unattended);
        let session_id = RemoteSessionId::from_raw(1);
        let peer = PeerIdentity {
            node_id: "node-1".into(),
            node_name: "test-node".into(),
            user_id: "user-1".into(),
        };

        // Observer requesting bidirectional audio
        let req = RequestedScope {
            role: SessionRole::Observer,
            displays: vec![0],
            audio: AudioScope::Bidirectional,
            clipboard: false,
            file_transfer: false,
        };

        let state = mgr
            .request_approval(session_id, peer, req.clone(), now)
            .unwrap();
        if let ApprovalState::Approved(grant) = state {
            // Clamped to PlaybackOnly, microphone strictly omitted!
            assert_eq!(grant.audio, AudioScope::PlaybackOnly);
        } else {
            panic!("expected approved");
        }

        // Observer requesting microphone only
        let session_id_2 = RemoteSessionId::from_raw(2);
        let peer_2 = PeerIdentity {
            node_id: "node-2".into(),
            node_name: "test-node-2".into(),
            user_id: "user-2".into(),
        };
        let req_mic = RequestedScope {
            role: SessionRole::Observer,
            displays: vec![0],
            audio: AudioScope::MicrophoneOnly,
            clipboard: false,
            file_transfer: false,
        };
        let state_2 = mgr
            .request_approval(session_id_2, peer_2, req_mic, now)
            .unwrap();
        if let ApprovalState::Approved(grant) = state_2 {
            // Clamped to None!
            assert_eq!(grant.audio, AudioScope::None);
        } else {
            panic!("expected approved");
        }

        // Controller requesting microphone gets it
        let session_id_3 = RemoteSessionId::from_raw(3);
        let peer_3 = PeerIdentity {
            node_id: "node-3".into(),
            node_name: "test-node-3".into(),
            user_id: "user-3".into(),
        };
        let req_ctrl = RequestedScope {
            role: SessionRole::Controller,
            displays: vec![0],
            audio: AudioScope::MicrophoneOnly,
            clipboard: false,
            file_transfer: false,
        };
        let state_3 = mgr
            .request_approval(session_id_3, peer_3, req_ctrl, now)
            .unwrap();
        if let ApprovalState::Approved(grant) = state_3 {
            assert_eq!(grant.audio, AudioScope::MicrophoneOnly);
        } else {
            panic!("expected approved");
        }
    }

    #[test]
    fn manual_approval_rejects_microphone_for_observer() {
        let now = HostInstant::from_micros(1000);
        let mut mgr = ApprovalManager::new(ApprovalMode::PromptAlways);
        let session_id = RemoteSessionId::from_raw(42);
        let peer = PeerIdentity {
            node_id: "node-42".into(),
            node_name: "test-node-42".into(),
            user_id: "user-42".into(),
        };

        let req = RequestedScope {
            role: SessionRole::Observer,
            displays: vec![0],
            audio: AudioScope::Bidirectional,
            clipboard: false,
            file_transfer: false,
        };
        mgr.request_approval(session_id, peer, req.clone(), now)
            .unwrap();

        // Attempt to grant microphone to observer
        let invalid_grant = GrantedScope {
            role: SessionRole::Observer,
            displays: vec![0],
            audio: AudioScope::Bidirectional,
            clipboard: false,
            file_transfer: false,
            granted_at: now,
            expires_at: None,
        };
        let err = mgr.approve(session_id, invalid_grant, now).unwrap_err();
        assert_eq!(err, DenialReason::PolicyForbidden);
    }
}
