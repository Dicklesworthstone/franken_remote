//! One immutable, bounded local machine-choice snapshot.
//!
//! A row retains only the exact stable node ID and canonical certificate name
//! to compare against a fresh installed-daemon lookup after selection. It has
//! no cached address, port, readiness, permission, or remote-session handle.
use super::saved_peers::{valid_id, valid_name};
use std::fmt;

/// The installed-daemon discovery parser's maximum peer count.
pub const MAX_CANDIDATES: usize = 1024;

/// Informational network-path evidence from the original discovery snapshot.
/// None of these values says the machine is online, running `FrankenRemote`,
/// ready to present a desktop, or authorized for observation or control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathHint {
    Direct,
    Relayed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidCandidate,
    Capacity,
    DuplicateIdentity,
    IndexOutOfRange,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Selection metadata only. Validation never normalizes or substitutes either
/// identity field: the displayed canonical name is the name to revalidate.
#[derive(Clone, PartialEq, Eq)]
pub struct Candidate {
    stable_id: Box<str>,
    certificate_name: Box<str>,
    path_hint: PathHint,
}
impl Candidate {
    pub fn new(
        stable_id: String,
        certificate_name: String,
        path_hint: PathHint,
    ) -> Result<Self, Error> {
        if !valid_id(&stable_id) || !valid_name(&certificate_name) {
            return Err(Error::InvalidCandidate);
        }
        // Drop spare caller capacity. Retained identity text is bounded by the
        // same 128-byte ID and 253-byte canonical-name limits as saved peers.
        Ok(Self {
            stable_id: stable_id.into_boxed_str(),
            certificate_name: certificate_name.into_boxed_str(),
            path_hint,
        })
    }
    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }
    pub fn certificate_name(&self) -> &str {
        &self.certificate_name
    }
    pub const fn path_hint(&self) -> PathHint {
        self.path_hint
    }
}
impl fmt::Debug for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Candidate")
            .field("path_hint", &self.path_hint)
            .finish_non_exhaustive()
    }
}

/// A deterministic snapshot whose row ordinals belong only to this snapshot.
/// Callers must retain this catalog through the chooser's decision and use the
/// chosen candidate's original ID/name tuple for a fresh pinned lookup. A new
/// discovery result must never reinterpret an old native row ordinal.
#[derive(Clone, PartialEq, Eq)]
pub struct Catalog {
    candidates: Box<[Candidate]>,
}
impl Catalog {
    /// Empty snapshots are valid and convey no selectable machine. Duplicate
    /// IDs or names refuse the entire snapshot, even if their path hints agree.
    pub fn new(mut candidates: Vec<Candidate>) -> Result<Self, Error> {
        if candidates.len() > MAX_CANDIDATES {
            return Err(Error::Capacity);
        }
        // Two bounded in-place sorts avoid a second identity set or trusting
        // caller iteration order to resolve conflicting rows.
        candidates.sort_unstable_by(|a, b| a.stable_id.cmp(&b.stable_id));
        if candidates
            .windows(2)
            .any(|rows| rows[0].stable_id == rows[1].stable_id)
        {
            return Err(Error::DuplicateIdentity);
        }
        candidates.sort_unstable_by(|a, b| {
            a.certificate_name
                .cmp(&b.certificate_name)
                .then_with(|| a.stable_id.cmp(&b.stable_id))
        });
        if candidates
            .windows(2)
            .any(|rows| rows[0].certificate_name == rows[1].certificate_name)
        {
            return Err(Error::DuplicateIdentity);
        }
        Ok(Self {
            candidates: candidates.into_boxed_slice(),
        })
    }
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }
    pub fn get(&self, index: usize) -> Result<&Candidate, Error> {
        self.candidates.get(index).ok_or(Error::IndexOutOfRange)
    }
    pub fn len(&self) -> usize {
        self.candidates.len()
    }
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }
}
impl fmt::Debug for Catalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Catalog")
            .field("count", &self.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, name: &str, path: PathHint) -> Candidate {
        Candidate::new(id.to_owned(), name.to_owned(), path).unwrap()
    }

    #[test]
    fn snapshot_sorts_without_rewriting_identity_or_path() {
        let first = candidate("n-Z9", "zulu.tail.ts.net", PathHint::Unknown);
        let second = candidate("n-A1", "alpha.tail.ts.net", PathHint::Relayed);
        let third = candidate("n-M5", "middle.tail.ts.net", PathHint::Direct);
        let catalog = Catalog::new(vec![first.clone(), second.clone(), third.clone()]).unwrap();
        let reversed = Catalog::new(vec![third, second, first]).unwrap();
        assert_eq!(catalog, reversed);
        assert_eq!(catalog.len(), 3);
        assert!(!catalog.is_empty());
        let selected: Vec<_> = catalog
            .candidates()
            .iter()
            .map(|row| (row.stable_id(), row.certificate_name(), row.path_hint()))
            .collect();
        assert_eq!(
            selected,
            [
                ("n-A1", "alpha.tail.ts.net", PathHint::Relayed),
                ("n-M5", "middle.tail.ts.net", PathHint::Direct),
                ("n-Z9", "zulu.tail.ts.net", PathHint::Unknown),
            ]
        );
    }

    #[test]
    fn empty_and_out_of_range_selections_refuse_without_fallback() {
        let empty = Catalog::new(Vec::new()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.get(0), Err(Error::IndexOutOfRange));
        let catalog = Catalog::new(vec![candidate(
            "n-one",
            "one.tail.ts.net",
            PathHint::Unknown,
        )])
        .unwrap();
        assert_eq!(catalog.get(0).unwrap().stable_id(), "n-one");
        assert_eq!(catalog.get(1), Err(Error::IndexOutOfRange));
        assert_eq!(catalog.get(usize::MAX), Err(Error::IndexOutOfRange));
        assert_eq!(catalog.len(), 1);
    }

    #[test]
    fn duplicate_ids_and_names_refuse_the_whole_snapshot() {
        for rows in [
            vec![
                candidate("n-same", "alpha.tail.ts.net", PathHint::Direct),
                candidate("n-other", "middle.tail.ts.net", PathHint::Unknown),
                candidate("n-same", "zulu.tail.ts.net", PathHint::Relayed),
            ],
            vec![
                candidate("n-a", "same.tail.ts.net", PathHint::Direct),
                candidate("n-b", "same.tail.ts.net", PathHint::Direct),
            ],
            vec![
                candidate("n-same", "same.tail.ts.net", PathHint::Unknown),
                candidate("n-same", "same.tail.ts.net", PathHint::Relayed),
            ],
        ] {
            assert_eq!(Catalog::new(rows), Err(Error::DuplicateIdentity));
        }
    }

    #[test]
    fn count_bound_accepts_the_full_snapshot_and_refuses_one_more() {
        let rows = |count| {
            (0..count)
                .map(|i| {
                    candidate(
                        &format!("n-{i:04}"),
                        &format!("host-{i:04}.tail.ts.net"),
                        PathHint::Unknown,
                    )
                })
                .collect()
        };
        let catalog = Catalog::new(rows(MAX_CANDIDATES)).unwrap();
        assert_eq!(catalog.len(), MAX_CANDIDATES);
        assert_eq!(
            catalog.get(MAX_CANDIDATES - 1).unwrap().stable_id(),
            "n-1023"
        );
        assert_eq!(Catalog::new(rows(MAX_CANDIDATES + 1)), Err(Error::Capacity));
    }

    #[test]
    fn invalid_ids_cannot_enter_the_snapshot() {
        for id in [
            "",
            "-node",
            "node name",
            "node\tname",
            "node\nname",
            "node\0name",
            "n/1",
            "n:1",
            "n?1",
            "n#1",
            "n@1",
            "n\\1",
            "n-é",
        ] {
            assert_eq!(
                Candidate::new(
                    id.to_owned(),
                    "host.tail.ts.net".to_owned(),
                    PathHint::Direct
                ),
                Err(Error::InvalidCandidate)
            );
        }
        let boundary = "N".repeat(128);
        assert_eq!(
            candidate(&boundary, "host.tail.ts.net", PathHint::Unknown).stable_id(),
            boundary
        );
        assert_eq!(
            Candidate::new(
                "N".repeat(129),
                "host.tail.ts.net".to_owned(),
                PathHint::Unknown
            ),
            Err(Error::InvalidCandidate)
        );
    }

    #[test]
    fn certificate_names_are_canonical_exact_and_bounded() {
        for name in [
            "",
            "host",
            "Host.tail.ts.net",
            "host.tail.ts.net.",
            ".tail.ts.net",
            "host..ts.net",
            "-host.ts.net",
            "host-.ts.net",
            "host_name.ts.net",
            "höst.ts.net",
            "host.ts.net:8443",
            "https://host.ts.net",
            "host.ts.net/",
            "host .ts.net",
            "host\n.ts.net",
            "host\0.ts.net",
        ] {
            assert_eq!(
                Candidate::new("n-host".to_owned(), name.to_owned(), PathHint::Unknown),
                Err(Error::InvalidCandidate)
            );
        }
        let longest = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        assert_eq!(longest.len(), 253);
        assert_eq!(
            candidate("n-max", &longest, PathHint::Unknown).certificate_name(),
            longest
        );
        for name in [format!("{}.ts.net", "a".repeat(64)), format!("{longest}d")] {
            assert_eq!(
                Candidate::new("n-host".to_owned(), name, PathHint::Unknown),
                Err(Error::InvalidCandidate)
            );
        }
    }

    #[test]
    fn a_new_snapshot_cannot_reinterpret_the_original_selection() {
        let mut id = "n-original".to_owned();
        let mut name = "middle.tail.ts.net".to_owned();
        let original = Catalog::new(vec![
            Candidate::new(id.clone(), name.clone(), PathHint::Direct).unwrap(),
        ])
        .unwrap();
        let retained = original.clone();
        id.clear();
        name.clear();
        let fresh = Catalog::new(vec![
            candidate("n-new", "alpha.tail.ts.net", PathHint::Relayed),
            original.get(0).unwrap().clone(),
        ])
        .unwrap();
        assert_eq!(fresh.get(0).unwrap().stable_id(), "n-new");
        assert_eq!(retained.get(0).unwrap().stable_id(), "n-original");
        assert_eq!(
            retained.get(0).unwrap().certificate_name(),
            "middle.tail.ts.net"
        );
        assert_eq!(retained.get(0).unwrap().path_hint(), PathHint::Direct);
    }

    #[test]
    fn debug_and_errors_never_include_peer_identity_text() {
        let row = candidate(
            "private-node-identity",
            "private-name.tail.ts.net",
            PathHint::Relayed,
        );
        let row_debug = format!("{row:?}");
        let catalog = Catalog::new(vec![row]).unwrap();
        let diagnostics = format!("{row_debug} {catalog:?} {}", Error::DuplicateIdentity);
        assert!(!diagnostics.contains("private-node-identity"));
        assert!(!diagnostics.contains("private-name.tail.ts.net"));
        assert!(diagnostics.contains("count: 1"));
        assert_eq!(Error::IndexOutOfRange.to_string(), "IndexOutOfRange");
    }
}
