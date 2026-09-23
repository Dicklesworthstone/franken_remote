//! Local, durable approval/sharing configuration, not a remote administration API.
//!
//! A saved policy applies at the NEXT daemon start unless the host explicitly
//! opts into `live::Watch`. Saving does not prove that a running host applied it.
//! Neither path authenticates a peer or approves an individual session.
//! Filesystem work belongs on the local CLI or an owned disk worker, not a reactor.
pub mod live;
pub mod options;
mod store;
pub use store::{Change, Saved, Store};

use serde::{Deserialize, Serialize};
use std::{fmt, io, path::PathBuf};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Approval {
    Local,
    /// The plan's optional local prompt is off by default. Own-user admission
    /// still applies; this setting never bypasses installed Tailscale identity.
    #[default]
    None,
}
impl Approval {
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "local" => Ok(Self::Local),
            "none" | "unattended" => Ok(Self::None),
            _ => Err(Error::InvalidArgument),
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::None => "unattended",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sharing {
    #[default]
    OwnUser,
    Tailnet,
}
impl Sharing {
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "own-user" => Ok(Self::OwnUser),
            "tailnet" => Ok(Self::Tailnet),
            _ => Err(Error::InvalidArgument),
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OwnUser => "own-user",
            Self::Tailnet => "tailnet",
        }
    }
}

/// Exact disk schema. Unknown/duplicate fields and unsupported versions refuse;
/// malformed or unreadable policy is NEVER replaced with permissive defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u32,
    /// Zero denotes unsaved plan defaults; stored revisions start at one.
    pub revision: u64,
    pub approval_mode: Approval,
    pub sharing_scope: Sharing,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            schema_version: 1,
            revision: 0,
            approval_mode: Approval::None,
            sharing_scope: Sharing::OwnUser,
        }
    }
}
impl Policy {
    fn validate(self) -> Result<Self, Error> {
        if self.schema_version != 1 || self.revision == 0 {
            return Err(Error::InvalidDocument);
        }
        Ok(self)
    }
}

/// Sanitized failures: no policy bytes, filesystem paths, or peer identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidArgument,
    InvalidPath,
    UnsafePath,
    InvalidDocument,
    TooLarge,
    Busy,
    RevisionExhausted,
    Io(io::ErrorKind),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value.kind())
    }
}
impl From<rustix::io::Errno> for Error {
    fn from(value: rustix::io::Errno) -> Self {
        io::Error::from(value).into()
    }
}

/// Per-user configuration by default. A system service running as root uses
/// /etc, never an inherited invoking user's HOME. Explicit paths must be absolute.
pub fn default_path() -> Result<PathBuf, Error> {
    if rustix::process::geteuid().is_root() {
        return Ok(PathBuf::from("/etc/frankenremote/host-policy.json"));
    }
    let base = if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        PathBuf::from(path)
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or(Error::InvalidPath)?).join(".config")
    };
    if !base.is_absolute() {
        return Err(Error::InvalidPath);
    }
    Ok(base.join("frankenremote/host-policy.json"))
}
