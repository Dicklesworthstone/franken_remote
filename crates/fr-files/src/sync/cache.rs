//! Bounded scan reuse within one original, descriptor-pinned folder job.
//!
//! Metadata detects unchanged entries without rereading their content when a
//! sibling changes. At most three partial scans may reuse a digest before every
//! file is hashed again; partial edits never refresh that verification age.
use super::{FolderManifest, SyncError, SyncLimits, source};
use crate::rx::{
    fd::{AsFd, BorrowedFd, OwnedFd},
    fs,
};

const MAX_REUSES: u8 = 3;

#[derive(Default)]
pub(super) struct ScanCache {
    snapshot: Option<Snapshot>,
}

struct Snapshot {
    // Retention prevents a recycled directory identity from adopting a cache.
    root: OwnedFd,
    scan: source::Scan,
    limits: SyncLimits,
    reuses: u8,
}

impl ScanCache {
    pub(super) fn scan<F>(
        &mut self,
        root: BorrowedFd<'_>,
        revision: u64,
        limits: SyncLimits,
        check: &mut F,
    ) -> Result<FolderManifest, SyncError>
    where
        F: FnMut() -> Result<(), SyncError>,
    {
        // Errors and cancellation discard all earlier reuse eligibility.
        let previous = self.snapshot.take();
        limits.validate()?;
        check()?;
        let reusable = if let Some(snapshot) = previous.as_ref()
            && snapshot.reuses < MAX_REUSES
            && snapshot.limits == limits
            && same_root(root, snapshot.root.as_fd())?
        {
            Some(snapshot)
        } else {
            None
        };
        let scan = source::scan_with_previous(
            root,
            revision,
            limits,
            reusable.map(|snapshot| &snapshot.scan),
            check,
        )?;
        let reuses = if scan.reused {
            // A changed sibling, changed name set or unchanged revision cannot
            // refresh the oldest retained digest's bounded verification age.
            let snapshot = reusable.expect("digest reuse requires the original snapshot");
            snapshot.reuses + 1
        } else {
            // Every entry was read, including empty and coarse-timestamp files.
            0
        };
        let manifest = scan.manifest.clone();
        let snapshot = Snapshot {
            root: root.try_clone_to_owned()?,
            scan,
            limits,
            reuses,
        };
        check()?;
        self.snapshot = Some(snapshot);
        Ok(manifest)
    }
}

fn same_root(current: BorrowedFd<'_>, original: BorrowedFd<'_>) -> Result<bool, SyncError> {
    let current = fs::fstat(current)?;
    let original = fs::fstat(original)?;
    Ok(current.st_dev == original.st_dev && current.st_ino == original.st_ino)
}

#[cfg(test)]
mod tests;
