//! Single-use restore token persistence and rotation for Linux portals (plan §10.1).
//!
//! Enforces:
//! 1. Atomic filesystem persistence: writes to temporary file with flush/sync,
//!    then atomically renames to destination.
//! 2. Single-use rotation: when a token is used to restore a session, the portal's
//!    response provides a replacement token (or none). The old token is invalidated/removed
//!    and the new token is persisted.
//! 3. Rejection recovery: if the compositor rejects a restore token (e.g. revoked
//!    or expired), the manager consumes the token and signals `PromptRequired` fallback
//!    rather than crashing or attempting unauthorized escalation.
//! 4. Strict input sanitization: tokens and session IDs are validated to prevent path traversal.

use core::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

/// Maximum allowed length for a restore token string.
pub const MAX_RESTORE_TOKEN_LEN: usize = 256;

/// Sanitized restore token entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreToken {
    pub token: String,
    pub session_id: String,
}

impl RestoreToken {
    pub fn new(session_id: &str, token: &str) -> Result<Self, RestoreTokenError> {
        validate_identifier(session_id)?;
        validate_token(token)?;
        Ok(Self {
            session_id: session_id.to_string(),
            token: token.to_string(),
        })
    }
}

/// Typed errors in restore token persistence and rotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreTokenError {
    InvalidIdentifier(String),
    InvalidTokenFormat(String),
    TokenNotFound(String),
    Io(String),
}

impl fmt::Display for RestoreTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier(id) => {
                write!(
                    f,
                    "invalid session identifier '{id}'; must be alphanumeric, hyphen, or underscore"
                )
            }
            Self::InvalidTokenFormat(msg) => write!(f, "invalid restore token format: {msg}"),
            Self::TokenNotFound(id) => write!(f, "no restore token found for session '{id}'"),
            Self::Io(err) => write!(f, "filesystem I/O error during token operation: {err}"),
        }
    }
}

impl std::error::Error for RestoreTokenError {}

/// Actionable fallback outcome when a restore token is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Token was valid and accepted by compositor. Replacement token persisted.
    RestoredAndRotated,
    /// Token was rejected or revoked by compositor. Old token removed, interactive prompt required.
    RejectedPromptRequired,
    /// No restore token existed. Interactive prompt required.
    FreshPromptRequired,
}

/// Manager for persisting and rotating portal restore tokens on disk.
pub struct RestoreTokenManager {
    storage_dir: PathBuf,
}

impl RestoreTokenManager {
    /// Create a manager with the specified storage directory.
    pub fn new(storage_dir: PathBuf) -> Result<Self, RestoreTokenError> {
        fs::create_dir_all(&storage_dir)
            .map_err(|e| RestoreTokenError::Io(format!("create storage dir: {e}")))?;
        Ok(Self { storage_dir })
    }

    /// Resolve destination file path for a session ID.
    fn token_path(&self, session_id: &str) -> Result<PathBuf, RestoreTokenError> {
        validate_identifier(session_id)?;
        Ok(self.storage_dir.join(format!("{session_id}.token")))
    }

    /// Atomically persist a token to disk.
    pub fn save_token_atomic(
        &self,
        session_id: &str,
        token: &str,
    ) -> Result<(), RestoreTokenError> {
        validate_identifier(session_id)?;
        validate_token(token)?;

        let dest_path = self.token_path(session_id)?;
        let tmp_path = self.storage_dir.join(format!(
            ".tmp_{}_{}_{}",
            session_id,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));

        // Write to temporary file with explicit sync
        let mut file = File::create(&tmp_path)
            .map_err(|e| RestoreTokenError::Io(format!("create temp token file: {e}")))?;
        file.write_all(token.as_bytes())
            .map_err(|e| RestoreTokenError::Io(format!("write token: {e}")))?;
        file.sync_all()
            .map_err(|e| RestoreTokenError::Io(format!("sync token file: {e}")))?;
        drop(file);

        // Atomic rename to final destination
        fs::rename(&tmp_path, &dest_path)
            .map_err(|e| RestoreTokenError::Io(format!("atomic rename token file: {e}")))?;

        Ok(())
    }

    /// Load stored token for a session ID, if present.
    pub fn load_token(&self, session_id: &str) -> Result<Option<RestoreToken>, RestoreTokenError> {
        let path = self.token_path(session_id)?;
        if !path.exists() {
            return Ok(None);
        }

        let raw = fs::read_to_string(&path)
            .map_err(|e| RestoreTokenError::Io(format!("read token: {e}")))?;
        let trimmed = raw.trim();
        validate_token(trimmed)?;

        Ok(Some(RestoreToken {
            session_id: session_id.to_string(),
            token: trimmed.to_string(),
        }))
    }

    /// Rotate or consume a token after attempting a portal restore.
    ///
    /// Single-use semantics:
    /// - If `replacement_token` is `Some(new_tok)`, the old token is replaced atomically by `new_tok`.
    /// - If `replacement_token` is `None`, the old token is deleted (consumed).
    pub fn rotate_token(
        &self,
        session_id: &str,
        replacement_token: Option<&str>,
    ) -> Result<Option<RestoreToken>, RestoreTokenError> {
        let path = self.token_path(session_id)?;

        if let Some(new_tok) = replacement_token {
            self.save_token_atomic(session_id, new_tok)?;
            Ok(Some(RestoreToken {
                session_id: session_id.to_string(),
                token: new_tok.to_string(),
            }))
        } else {
            if path.exists() {
                let _ = fs::remove_file(&path);
            }
            Ok(None)
        }
    }

    /// Handle rejection of a restore token by the compositor.
    ///
    /// Consumes/deletes the invalid token and returns `RestoreOutcome::RejectedPromptRequired`.
    pub fn handle_rejection(&self, session_id: &str) -> RestoreOutcome {
        if let Ok(path) = self.token_path(session_id)
            && path.exists()
        {
            let _ = fs::remove_file(path);
        }
        RestoreOutcome::RejectedPromptRequired
    }
}

/// Validate identifier to prevent path traversal and shell injection.
fn validate_identifier(id: &str) -> Result<(), RestoreTokenError> {
    if id.is_empty() || id.len() > 64 {
        return Err(RestoreTokenError::InvalidIdentifier(id.to_string()));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(RestoreTokenError::InvalidIdentifier(id.to_string()));
    }
    Ok(())
}

/// Validate token format.
fn validate_token(token: &str) -> Result<(), RestoreTokenError> {
    if token.is_empty() {
        return Err(RestoreTokenError::InvalidTokenFormat(
            "token cannot be empty".to_string(),
        ));
    }
    if token.len() > MAX_RESTORE_TOKEN_LEN {
        return Err(RestoreTokenError::InvalidTokenFormat(format!(
            "token length {} exceeds maximum {}",
            token.len(),
            MAX_RESTORE_TOKEN_LEN
        )));
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(RestoreTokenError::InvalidTokenFormat(
            "token contains invalid characters".to_string(),
        ));
    }
    Ok(())
}
