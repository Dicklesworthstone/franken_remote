//! A scanned source version must match the independently prepared ATP object.
//! These receipts contain no authorization and cannot migrate between jobs.
use super::{Error, TransferManifest};
use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ExpectedSource {
    File {
        size: u64,
        sha256: String,
    },
    Tree {
        size: u64,
        merkle_root: String,
        metadata_root: Option<String>,
    },
}

impl ExpectedSource {
    pub(crate) const fn size(&self) -> u64 {
        match self {
            Self::File { size, .. } | Self::Tree { size, .. } => *size,
        }
    }

    pub(super) const fn directory(&self) -> bool {
        matches!(self, Self::Tree { .. })
    }

    pub(super) fn validate(&self) -> Result<(), Error> {
        let valid = match self {
            Self::File { sha256, .. } => lower_hash(sha256),
            Self::Tree {
                merkle_root,
                metadata_root,
                ..
            } => lower_hash(merkle_root) && metadata_root.as_deref().is_none_or(lower_hash),
        };
        if valid { Ok(()) } else { Err(Error::Source) }
    }

    /// Kind and structural metadata belong to the version: an empty directory
    /// and an empty regular file can share content but are never interchangeable.
    pub(crate) fn ledger_key(&self) -> String {
        match self {
            Self::File { size, sha256 } => format!("file:{size}:{sha256}"),
            Self::Tree {
                size,
                merkle_root,
                metadata_root,
            } => format!(
                "tree:{size}:{merkle_root}:{}",
                metadata_root.as_deref().unwrap_or("-")
            ),
        }
    }

    pub(super) fn matches(&self, manifest: &TransferManifest) -> bool {
        match self {
            Self::File { size, sha256 } => {
                !manifest.is_directory
                    && manifest.entries.len() == 1
                    && manifest.total_bytes == *size
                    && manifest.entries[0].sha256_hex == *sha256
                    && manifest.metadata_root_hex.is_none()
            }
            Self::Tree {
                size,
                merkle_root,
                metadata_root,
            } => {
                manifest.is_directory
                    && manifest.total_bytes == *size
                    && manifest.merkle_root_hex == *merkle_root
                    && manifest.metadata_root_hex == *metadata_root
            }
        }
    }

    pub(super) fn from_manifest(manifest: &TransferManifest) -> Result<Self, Error> {
        let version = if manifest.is_directory {
            Self::Tree {
                size: manifest.total_bytes,
                merkle_root: manifest.merkle_root_hex.clone(),
                metadata_root: manifest.metadata_root_hex.clone(),
            }
        } else {
            let [entry] = manifest.entries.as_slice() else {
                return Err(Error::Source);
            };
            if entry.size != manifest.total_bytes || manifest.metadata_root_hex.is_some() {
                return Err(Error::Source);
            }
            Self::File {
                size: entry.size,
                sha256: entry.sha256_hex.clone(),
            }
        };
        version.validate()?;
        Ok(version)
    }
}

impl fmt::Debug for ExpectedSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExpectedSource")
            .field("directory", &self.directory())
            .field("size", &self.size())
            .finish_non_exhaustive()
    }
}

fn lower_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
