//! Explicit local standing-job consent; no transport or authority is fabricated.
use super::*;
use crate::native_files::{ConflictPolicy, Directory, Limits};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "fr-native-upload-job-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
    fn directory(&self) -> Directory {
        Directory::open(&self.0, Limits::default()).unwrap()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn source_consent_is_one_use_across_clones_and_opens_no_entry() {
    let root = Scratch::new();
    // A source root can be chosen before scanning; entries are inspected only
    // after the original host advertises the explicitly accepted job.
    std::os::unix::fs::symlink("missing-entry", root.0.join("unsupported-entry")).unwrap();
    let selected = UploadSelection::folder_job(&root.0, 41).unwrap();
    let clone = selected.clone();
    assert_eq!(selected.sync_job_id(), Some(41));
    assert!(selected.is_available() && clone.is_available());
    assert!(!format!("{selected:?}").contains(root.0.to_str().unwrap()));
    let config = selected.take().unwrap();
    assert_eq!(config.job_id(), 41);
    assert!(!clone.is_available());
    assert_eq!(clone.sync_job_id(), None);
    assert_eq!(clone.take().unwrap_err(), Absence::Unavailable);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
    assert_eq!(
        UploadSelection::folder_job(&root.0, 0).unwrap_err(),
        LiveError::InvalidConfig
    );
}

#[test]
fn host_destination_requires_keep_both_and_spent_clones_never_fall_back() {
    let root = Scratch::new();
    assert_eq!(
        root.directory().with_sync_job(41).unwrap_err(),
        LiveError::KeepBothRequired
    );
    assert_eq!(
        root.directory()
            .with_conflict_policy(ConflictPolicy::KeepBoth)
            .with_sync_job(0)
            .unwrap_err(),
        LiveError::InvalidConfig
    );
    let selected = root
        .directory()
        .with_conflict_policy(ConflictPolicy::KeepBoth)
        .with_sync_job(41)
        .unwrap();
    let clone = selected.clone();
    assert!(selected.requires_sync_job());
    assert_eq!(clone.sync_job_id(), Some(41));
    // A changed preference cannot consume standing consent or weaken KeepBoth.
    let rejected = clone.clone().with_conflict_policy(ConflictPolicy::Reject);
    assert_eq!(rejected.take_sync_job(), Err(Absence::Unavailable));
    assert_eq!(selected.sync_job_id(), Some(41));
    assert_eq!(
        selected.clone().with_sync_job(42).unwrap_err(),
        LiveError::InvalidConfig
    );
    assert_eq!(selected.take_sync_job(), Ok(Some(41)));
    assert!(clone.requires_sync_job());
    assert_eq!(clone.sync_job_id(), None);
    assert_eq!(clone.take_sync_job(), Err(Absence::Unavailable));
    // Ordinary receive consent remains ordinary and does not spend a job.
    assert_eq!(root.directory().take_sync_job(), Ok(None));
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0);
}
