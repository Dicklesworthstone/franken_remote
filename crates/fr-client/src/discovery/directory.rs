//! Optional authenticated directory service and policy enforcement.
//!
//! Per plan section 6.4:
//! - Mobile apps and web pages can use an optional authenticated directory served
//!   by a known `frd`.
//! - Opt-in and limited by local policy (`Disabled`, `RestrictedOwnUserOnly`, `TailnetMembers`).
//! - A directory response is discovery information only; it NEVER delegates permission
//!   to control other hosts and NEVER proxies connections around Tailscale policy.
//! - Strict privacy invariant: Responses NEVER carry screenshots, window/application
//!   titles, clipboard previews, or audio.

use serde::{Deserialize, Serialize};
use std::{fmt, net::IpAddr};

/// Maximum number of records retained in the directory service.
pub const MAX_DIRECTORY_RECORDS: usize = 256;

/// Mandatory disclaimer included with every directory response.
pub const DIRECTORY_DISCLAIMER: &str =
    "Discovery information only; does not delegate permission or proxy connectivity.";

/// Local policy governing directory exposure.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DirectoryPolicy {
    /// Directory service is completely disabled (default).
    #[default]
    Disabled,
    /// Directory responses only expose machines owned by the requesting user.
    RestrictedOwnUserOnly {
        /// Owner user ID (e.g. "alice@example.com" or Tailscale user ID).
        owner_user_id: String,
    },
    /// Directory responses expose enrolled machines to verified tailnet members.
    TailnetMembers {
        /// Enrolled tailnet domain/ID.
        tailnet_name: String,
    },
}

/// Reason for refusing a directory discovery request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectoryRefusalReason {
    /// Directory service is disabled by host policy.
    DirectoryDisabled,
    /// Requester does not match the configured owner user ID.
    RestrictedToOwnUser,
    /// Requester belongs to an unauthorized or non-matching tailnet.
    TailnetMismatch,
    /// Requester identity could not be verified.
    UnverifiedIdentity,
}

impl fmt::Display for DirectoryRefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectoryDisabled => write!(f, "directory service is disabled on this host"),
            Self::RestrictedToOwnUser => {
                write!(f, "directory is restricted to owner user inventory only")
            }
            Self::TailnetMismatch => write!(f, "requester is not from the authorized tailnet"),
            Self::UnverifiedIdentity => write!(f, "requester identity is unverified"),
        }
    }
}

impl std::error::Error for DirectoryRefusalReason {}

/// Non-sensitive host record exposed via authenticated directory.
///
/// # Privacy Invariant
/// This struct explicitly NEVER contains screenshots, window/application titles,
/// clipboard previews, or audio streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryHostRecord {
    /// Stable Tailscale node ID.
    pub stable_id: String,
    /// Tailnet DNS / certificate name.
    pub certificate_name: String,
    /// Advertised IP addresses.
    pub addresses: Vec<IpAddr>,
    /// Service port.
    pub port: u16,
    /// Owner user ID of this machine.
    pub owner_user_id: String,
    /// Whether host is currently reporting as active/ready.
    pub is_active: bool,
}

impl DirectoryHostRecord {
    /// Create a new directory host record.
    pub fn new(
        stable_id: impl Into<String>,
        certificate_name: impl Into<String>,
        addresses: Vec<IpAddr>,
        port: u16,
        owner_user_id: impl Into<String>,
        is_active: bool,
    ) -> Self {
        Self {
            stable_id: stable_id.into(),
            certificate_name: certificate_name.into(),
            addresses,
            port,
            owner_user_id: owner_user_id.into(),
            is_active,
        }
    }
}

/// Incoming authenticated directory discovery query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryRequest {
    /// Verified Tailscale user identity of the requester.
    pub requester_user_id: String,
    /// Verified Tailscale domain/tailnet of the requester.
    pub requester_tailnet: String,
}

/// Directory discovery response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryResponse {
    /// Discovered host records adhering to directory policy.
    pub records: Vec<DirectoryHostRecord>,
    /// Statutory disclaimer confirming no permission delegation or proxying.
    pub disclaimer: &'static str,
}

/// Authenticated directory service managing host inventory and policy enforcement.
#[derive(Debug, Clone)]
pub struct DirectoryService {
    policy: DirectoryPolicy,
    records: Vec<DirectoryHostRecord>,
}

impl Default for DirectoryService {
    fn default() -> Self {
        Self::new(DirectoryPolicy::Disabled)
    }
}

impl DirectoryService {
    /// Create a new directory service with specified policy.
    #[must_use]
    pub fn new(policy: DirectoryPolicy) -> Self {
        Self {
            policy,
            records: Vec::new(),
        }
    }

    /// Current policy.
    #[must_use]
    pub fn policy(&self) -> &DirectoryPolicy {
        &self.policy
    }

    /// Update policy.
    pub fn set_policy(&mut self, policy: DirectoryPolicy) {
        self.policy = policy;
    }

    /// Number of enrolled host records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether directory has no enrolled records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Register a host in the directory.
    pub fn register_host(&mut self, record: DirectoryHostRecord) -> Result<(), &'static str> {
        if self.records.len() >= MAX_DIRECTORY_RECORDS {
            return Err("directory capacity exceeded (max 256)");
        }
        if let Some(existing) = self
            .records
            .iter_mut()
            .find(|r| r.stable_id == record.stable_id)
        {
            *existing = record;
            return Ok(());
        }
        self.records.push(record);
        Ok(())
    }

    /// Unregister a host from the directory.
    pub fn unregister_host(&mut self, stable_id: &str) -> bool {
        if let Some(pos) = self.records.iter().position(|r| r.stable_id == stable_id) {
            self.records.remove(pos);
            true
        } else {
            false
        }
    }

    /// Query the directory under current policy.
    ///
    /// Evaluates requester credentials against the active `DirectoryPolicy`.
    pub fn query(
        &self,
        request: &DirectoryRequest,
    ) -> Result<DirectoryResponse, DirectoryRefusalReason> {
        match &self.policy {
            DirectoryPolicy::Disabled => Err(DirectoryRefusalReason::DirectoryDisabled),
            DirectoryPolicy::RestrictedOwnUserOnly { owner_user_id } => {
                if request.requester_user_id.is_empty() {
                    return Err(DirectoryRefusalReason::UnverifiedIdentity);
                }
                if request.requester_user_id != *owner_user_id {
                    return Err(DirectoryRefusalReason::RestrictedToOwnUser);
                }
                let records = self
                    .records
                    .iter()
                    .filter(|r| r.owner_user_id == *owner_user_id)
                    .cloned()
                    .collect();
                Ok(DirectoryResponse {
                    records,
                    disclaimer: DIRECTORY_DISCLAIMER,
                })
            }
            DirectoryPolicy::TailnetMembers { tailnet_name } => {
                if request.requester_tailnet.is_empty() {
                    return Err(DirectoryRefusalReason::UnverifiedIdentity);
                }
                if request.requester_tailnet != *tailnet_name {
                    return Err(DirectoryRefusalReason::TailnetMismatch);
                }
                Ok(DirectoryResponse {
                    records: self.records.clone(),
                    disclaimer: DIRECTORY_DISCLAIMER,
                })
            }
        }
    }
}
