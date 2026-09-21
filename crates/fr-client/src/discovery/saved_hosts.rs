//! Saved hosts store and explicit host links parsing with strict bearer token rejection.
//!
//! Per plan section 6.4 and 6.5:
//! - Mobile apps and web baselines use saved hosts and explicit host links (`fr://<host>[:<port>]`).
//! - No long-lived bearer tokens are placed in host links.
//! - Bearer credentials, userinfo, and token parameters are unconditionally rejected.
//! - Bounded storage capacity (max 100 entries).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Maximum number of saved hosts retained in the store.
pub const MAX_SAVED_HOSTS: usize = 100;

/// Default service port for `FrankenRemote`.
pub const DEFAULT_SERVICE_PORT: u16 = 8443;

/// Errors arising from parsing or validating a host link (`fr://...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLinkError {
    /// Scheme is not `fr` or `frs`.
    InvalidScheme,
    /// Host name or address is missing.
    MissingHost,
    /// Port number is invalid or outside 1..=65535.
    InvalidPort,
    /// Bearer material, token, password, or credential detected in URL.
    /// This is an unconditional rejection per security rules.
    BearerMaterialForbidden,
    /// Malformed URI syntax.
    MalformedUri,
}

impl fmt::Display for HostLinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScheme => write!(f, "invalid scheme (must be fr:// or frs://)"),
            Self::MissingHost => write!(f, "missing host in link"),
            Self::InvalidPort => write!(f, "invalid port in link"),
            Self::BearerMaterialForbidden => {
                write!(f, "bearer tokens or credentials forbidden in host link")
            }
            Self::MalformedUri => write!(f, "malformed host link URI"),
        }
    }
}

impl std::error::Error for HostLinkError {}

/// Parsed explicit host link (`fr://<host>[:<port>][?display=<idx>]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLink {
    /// Destination host name or IP address.
    pub host: String,
    /// Port number.
    pub port: u16,
    /// Whether link explicitly requested TLS transport (`frs://`).
    pub secure: bool,
    /// Optional display index specified in query (`?display=N`).
    pub display_index: Option<u32>,
}

impl HostLink {
    /// Parse an explicit host link string.
    ///
    /// # Security Invariant
    /// Any userinfo (`fr://token@host` or `fr://user:pass@host`) or credential query
    /// parameter (`?token=`, `?key=`, `?auth=`, `?secret=`, etc.) triggers
    /// `HostLinkError::BearerMaterialForbidden`.
    pub fn parse(raw: &str) -> Result<Self, HostLinkError> {
        let trimmed = raw.trim();

        // Scheme validation
        let (secure, rest) = if let Some(stripped) = trimmed.strip_prefix("frs://") {
            (true, stripped)
        } else if let Some(stripped) = trimmed.strip_prefix("fr://") {
            (false, stripped)
        } else {
            return Err(HostLinkError::InvalidScheme);
        };

        if rest.is_empty() {
            return Err(HostLinkError::MissingHost);
        }

        // Check for userinfo (@) which indicates embedded tokens/credentials
        if rest.contains('@') {
            return Err(HostLinkError::BearerMaterialForbidden);
        }

        // Split host/port from path and query
        let (authority, query) = match rest.split_once('?') {
            Some((auth_part, q)) => (auth_part, Some(q)),
            None => (rest, None),
        };

        // Strip optional trailing path (e.g. "host:8443/")
        let authority = authority.split('/').next().unwrap_or(authority);
        if authority.is_empty() {
            return Err(HostLinkError::MissingHost);
        }

        // Parse host and optional port, handling IPv6 bracket syntax `[::1]:8443`
        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            // IPv6 literal
            let (ipv6_addr, after_bracket) = stripped
                .split_once(']')
                .ok_or(HostLinkError::MalformedUri)?;
            if ipv6_addr.is_empty() {
                return Err(HostLinkError::MissingHost);
            }
            let port = if let Some(port_str) = after_bracket.strip_prefix(':') {
                port_str
                    .parse::<u16>()
                    .map_err(|_| HostLinkError::InvalidPort)?
            } else if after_bracket.is_empty() {
                DEFAULT_SERVICE_PORT
            } else {
                return Err(HostLinkError::InvalidPort);
            };
            (ipv6_addr.to_string(), port)
        } else {
            // Hostname or IPv4
            match authority.split_once(':') {
                Some((h, p)) => {
                    if h.is_empty() {
                        return Err(HostLinkError::MissingHost);
                    }
                    let port = p.parse::<u16>().map_err(|_| HostLinkError::InvalidPort)?;
                    (h.to_string(), port)
                }
                None => (authority.to_string(), DEFAULT_SERVICE_PORT),
            }
        };

        // Inspect query parameters for forbidden credentials or allowed display index
        let mut display_index = None;
        if let Some(query_str) = query {
            for param in query_str.split('&') {
                if param.is_empty() {
                    continue;
                }
                let (key, value) = match param.split_once('=') {
                    Some((k, v)) => (k, v),
                    None => (param, ""),
                };

                let lower_key = key.to_ascii_lowercase();
                // Prohibit any credential or bearer token parameters
                if lower_key.contains("token")
                    || lower_key.contains("auth")
                    || lower_key.contains("key")
                    || lower_key.contains("secret")
                    || lower_key.contains("pass")
                    || lower_key.contains("cred")
                    || lower_key.contains("bearer")
                    || lower_key.contains("session")
                {
                    return Err(HostLinkError::BearerMaterialForbidden);
                }

                if lower_key == "display" {
                    let idx = value
                        .parse::<u32>()
                        .map_err(|_| HostLinkError::MalformedUri)?;
                    display_index = Some(idx);
                }
            }
        }

        Ok(Self {
            host,
            port,
            secure,
            display_index,
        })
    }
}

/// A persistent saved host machine entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHost {
    /// Unique identifier for this saved entry (e.g. UUID).
    pub id: String,
    /// User-assigned label (e.g. "Office Desktop", "Mac mini").
    pub label: String,
    /// Destination host FQDN or IP string.
    pub host: String,
    /// Destination port.
    pub port: u16,
    /// Host monotonic timestamp when added.
    pub added_at_us: u64,
    /// Timestamp of most recent successful session, if any.
    pub last_connected_us: Option<u64>,
}

impl SavedHost {
    /// Create a new saved host entry.
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        now_us: u64,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            host: host.into(),
            port,
            added_at_us: now_us,
            last_connected_us: None,
        }
    }
}

/// Errors from operating on the `SavedHostStore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedHostError {
    /// Capacity exceeded (`MAX_SAVED_HOSTS`).
    CapacityExceeded,
    /// A host with this ID or host:port already exists.
    DuplicateHost,
    /// Specified host was not found.
    HostNotFound,
    /// Serialization or deserialization failure.
    SerializationError(String),
}

impl fmt::Display for SavedHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExceeded => write!(f, "saved hosts capacity exceeded (max 100)"),
            Self::DuplicateHost => write!(f, "duplicate host entry"),
            Self::HostNotFound => write!(f, "saved host not found"),
            Self::SerializationError(e) => write!(f, "saved host serialization error: {e}"),
        }
    }
}

impl std::error::Error for SavedHostError {}

/// Bounded collection of saved hosts with JSON persistence support.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedHostStore {
    hosts: Vec<SavedHost>,
}

impl SavedHostStore {
    /// Create an empty saved host store.
    #[must_use]
    pub fn new() -> Self {
        Self { hosts: Vec::new() }
    }

    /// Number of saved hosts in store.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    /// Whether store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// List all saved hosts.
    #[must_use]
    pub fn list(&self) -> &[SavedHost] {
        &self.hosts
    }

    /// Find a saved host by ID.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&SavedHost> {
        self.hosts.iter().find(|h| h.id == id)
    }

    /// Find a saved host by ID (mutable).
    pub fn get_mut(&mut self, id: &str) -> Option<&mut SavedHost> {
        self.hosts.iter_mut().find(|h| h.id == id)
    }

    /// Add a new saved host.
    pub fn add(&mut self, host: SavedHost) -> Result<(), SavedHostError> {
        if self.hosts.len() >= MAX_SAVED_HOSTS {
            return Err(SavedHostError::CapacityExceeded);
        }
        if self.hosts.iter().any(|h| h.id == host.id) {
            return Err(SavedHostError::DuplicateHost);
        }
        if self
            .hosts
            .iter()
            .any(|h| h.host.eq_ignore_ascii_case(&host.host) && h.port == host.port)
        {
            return Err(SavedHostError::DuplicateHost);
        }
        self.hosts.push(host);
        Ok(())
    }

    /// Remove a saved host by ID.
    pub fn remove(&mut self, id: &str) -> bool {
        if let Some(pos) = self.hosts.iter().position(|h| h.id == id) {
            self.hosts.remove(pos);
            true
        } else {
            false
        }
    }

    /// Update `last_connected_us` timestamp for a saved host.
    pub fn record_connection(&mut self, id: &str, now_us: u64) -> bool {
        if let Some(host) = self.hosts.iter_mut().find(|h| h.id == id) {
            host.last_connected_us = Some(now_us);
            true
        } else {
            false
        }
    }

    /// Serialize store to JSON string.
    pub fn to_json(&self) -> Result<String, SavedHostError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| SavedHostError::SerializationError(e.to_string()))
    }

    /// Deserialize store from JSON string.
    pub fn from_json(json: &str) -> Result<Self, SavedHostError> {
        let store: Self = serde_json::from_str(json)
            .map_err(|e| SavedHostError::SerializationError(e.to_string()))?;
        if store.hosts.len() > MAX_SAVED_HOSTS {
            return Err(SavedHostError::CapacityExceeded);
        }
        Ok(store)
    }
}
