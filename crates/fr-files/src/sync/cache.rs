//! Bounded idle-scan reuse within one original, descriptor-pinned folder job.
//!
//! Metadata is a change detector, never permanent proof of content equality.
//! After at most three unchanged scans, the canonical scanner reads every byte
//! again. Any change or uncertainty also takes that original full-scan path.
use super::{FolderManifest, SyncError, SyncLimits, source};
use crate::rx::{
    MetadataExt,
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
    manifest: FolderManifest,
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
        // Failed or cancelled scans never leave an older snapshot eligible for
        // reuse. A candidate is installed only after the final authority check.
        let previous = self.snapshot.take();
        limits.validate()?;
        check()?;
        if let Some(mut snapshot) = previous
            && snapshot.reuses < MAX_REUSES
            && snapshot.limits == limits
            && same_root(root, snapshot.root.as_fd())?
            && unchanged_directory(root, &snapshot.manifest, limits, check)?
        {
            let mut manifest = snapshot.manifest.clone();
            manifest.revision = revision;
            snapshot.reuses += 1;
            check()?;
            self.snapshot = Some(snapshot);
            return Ok(manifest);
        }

        let manifest = source::scan(root, revision, limits, check)?;
        let snapshot = Snapshot {
            root: root.try_clone_to_owned()?,
            manifest: manifest.clone(),
            limits,
            reuses: 0,
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

fn unchanged_directory<F>(
    root: BorrowedFd<'_>,
    previous: &FolderManifest,
    limits: SyncLimits,
    check: &mut F,
) -> Result<bool, SyncError>
where
    F: FnMut() -> Result<(), SyncError>,
{
    check()?;
    let mut directory = fs::Dir::read_from(root)?;
    let mut names = Vec::new();
    let mut inspected = 0u32;
    loop {
        check()?;
        let Some(entry) = directory.read() else { break };
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| SyncError::InvalidEntryName)?;
        if matches!(name, "." | "..") {
            continue;
        }
        // Private staging entries still consume the original enumeration bound.
        if inspected == limits.max_entries {
            return Err(SyncError::EntryLimitExceeded);
        }
        inspected += 1;
        if name.starts_with(super::STAGING_PREFIX) {
            continue;
        }
        crate::receive::validate_name(name).map_err(|_| SyncError::InvalidEntryName)?;
        names.push(name.to_owned());
    }
    names.sort();
    if inspected != previous.inspected_entries || names.len() != previous.entries.len() {
        return Ok(false);
    }
    for name in names {
        check()?;
        let Some(expected) = previous.entries.get(&name) else {
            return Ok(false);
        };
        // select checks regular type, NOFOLLOW/NONBLOCK, and the complete
        // original dev/inode/size/mtime/ctime signature including nanoseconds.
        let Ok(first) = source::select(root, expected) else {
            return Ok(false);
        };
        let metadata = first.metadata()?;
        // Empty or coarse/ambiguous files keep the canonical EOF/content probe.
        if metadata.len() == 0
            || metadata.mtime() <= 0
            || metadata.ctime() <= 0
            || !(1..1_000_000_000).contains(&metadata.mtime_nsec())
            || !(1..1_000_000_000).contains(&metadata.ctime_nsec())
        {
            return Ok(false);
        }
        check()?;
        // Recheck the original signature after the callback, retaining the
        // first descriptor until the second checked open has completed.
        let second = source::select(root, expected);
        drop(first);
        if second.is_err() {
            return Ok(false);
        }
        check()?;
    }
    check()?;
    Ok(true)
}

#[cfg(test)]
mod tests;
