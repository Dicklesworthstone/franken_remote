//! Local publication policy. A remote manifest never selects this behavior.
use super::Publication;
use std::fmt;

/// Locally chosen behavior for an occupied destination. Both policies preserve
/// existing files, directories and symlinks, including concurrent arrivals.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Refuse a conflicting publication. This is the backwards-compatible default.
    #[default]
    Reject,
    /// Atomically keep the new file under a clearly marked, bounded conflict name.
    /// Never replaces or renames the existing entry, and never retries a transfer.
    KeepBoth,
}

/// Local result of a publication, not a diagnostic or a wire receipt. The name
/// identifies the entry in the originally selected directory, not a reopened path.
/// Both durability variants mean the file was published; neither permits replay.
pub struct PublishedFile {
    pub(super) name: String,
    pub(super) publication: Publication,
    pub(super) renamed: bool,
}
impl PublishedFile {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn publication(&self) -> Publication {
        self.publication
    }
    pub const fn was_renamed(&self) -> bool {
        self.renamed
    }
}
impl fmt::Debug for PublishedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublishedFile")
            .field("publication", &self.publication)
            .field("renamed", &self.renamed)
            .finish_non_exhaustive()
    }
}

pub(super) const MAX_CONFLICT_ATTEMPTS: u8 = 8;
/// The nonce is the random identity already allocated for this staging owner.
/// Failed candidates remain no-overwrite operations, with finite retry work.
pub(super) fn conflict_name(original: &str, nonce: u128, attempt: u8) -> String {
    let marker = format!(".fr-conflict-{nonce:032x}-{attempt}");
    // Preserve ordinary extensions for local usability, but do not allow an
    // adversarially long extension to consume the finite basename budget.
    let (stem, extension) = match original.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && extension.len() <= 32 => {
            (stem, &original[stem.len()..])
        }
        _ => (original, ""),
    };
    let mut end = stem.len().min(255 - marker.len() - extension.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}{extension}", &stem[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::atp::safety::validate_portable_path_component;

    #[test]
    fn conflict_names_are_portable_bounded_utf8_and_keep_normal_extensions() {
        for original in [
            "report.txt".into(),
            ".profile".into(),
            "é".repeat(127),
            format!("{}.txt", "漢".repeat(83)),
            "a".repeat(255),
            format!("a.{}", "z".repeat(253)),
        ] {
            for attempt in 0..MAX_CONFLICT_ATTEMPTS {
                let name = conflict_name(&original, 17, attempt);
                assert!(name.len() <= 255);
                assert!(name.contains(".fr-conflict-"));
                assert_ne!(name, original);
                assert!(validate_portable_path_component(&name).is_ok());
            }
        }
        assert_eq!(
            std::path::Path::new(&conflict_name("report.txt", 17, 0))
                .extension()
                .unwrap(),
            "txt"
        );
        assert_ne!(
            conflict_name("report.txt", 17, 0),
            conflict_name("report.txt", 17, 1)
        );
        assert_ne!(
            conflict_name("report.txt", 17, 0),
            conflict_name("report.txt", 18, 0)
        );
    }
}
