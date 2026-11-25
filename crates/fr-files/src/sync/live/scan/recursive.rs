//! Recursive jobs retain atomic top-level versions, not writable peer paths.
//!
//! Every descendant is inspected and hashed by the ordinary ATP source walker.
//! Only one tree's descriptors live during inspection; at most one delivery
//! page's top-level descriptors survive into the existing sender owner.
use super::{PreparedRound, ScanCache, Version, paging};
use crate::{
    rx::{
        MetadataExt,
        fd::BorrowedFd,
        fs::{self, AtFlags, FileType, Mode, OFlags},
    },
    sender::{self, ExpectedSource, batch::Selection},
    sync::{SyncError, SyncLimits, live::LiveError, source},
};
use std::{collections::BTreeMap, fs::File};

struct Entry {
    original: source::Identity,
    version: ExpectedSource,
    key: String,
}

pub(super) fn prepare(
    root: BorrowedFd<'_>,
    limits: SyncLimits,
    ledger: &BTreeMap<String, String>,
    after: Option<&str>,
    check: &mut impl FnMut() -> Result<(), SyncError>,
) -> Result<PreparedRound, LiveError> {
    limits.validate().map_err(LiveError::Source)?;
    let entries = inspect(root, limits, check)?;
    let page = paging::select(
        entries
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.key.as_str())),
        ledger,
        after,
    );
    let mut selection = Selection::new();
    let mut versions = Vec::with_capacity(page.names.len());
    let mut bytes = 0_u64;
    for name in page.names {
        check().map_err(LiveError::Source)?;
        let entry = entries.get(name).expect("page retains the inspected entry");
        let file = open_entry(root, name).map_err(LiveError::Source)?;
        source::unchanged(&file, &entry.original).map_err(LiveError::Source)?;
        check().map_err(LiveError::Source)?;
        let size = entry.version.size();
        selection
            .push_version(file, name, entry.version.clone())
            .map_err(LiveError::Send)?;
        bytes = bytes.checked_add(size).ok_or(LiveError::Resource)?;
        versions.push(Version {
            name: name.to_owned(),
            hash: entry.key.clone(),
            bytes: size,
        });
    }
    check().map_err(LiveError::Source)?;
    Ok(PreparedRound {
        selection,
        versions,
        bytes,
        // A directory's own timestamps do not describe child content changes.
        // Recursive scans deliberately do not borrow the flat metadata cache.
        cache: ScanCache::default(),
        continuation: page.continuation,
    })
}

fn inspect(
    root: BorrowedFd<'_>,
    limits: SyncLimits,
    check: &mut impl FnMut() -> Result<(), SyncError>,
) -> Result<BTreeMap<String, Entry>, LiveError> {
    let names = source::entry_names(root, limits, check).map_err(LiveError::Source)?;
    let mut inspected = names.inspected;
    let mut total_bytes = 0_u64;
    let mut entries = BTreeMap::new();
    for name in names.values {
        check().map_err(LiveError::Source)?;
        let file = open_entry(root, &name).map_err(LiveError::Source)?;
        let metadata = file
            .metadata()
            .map_err(SyncError::from)
            .map_err(LiveError::Source)?;
        let original = source::identity(&metadata);
        let directory = metadata.is_dir();
        if !directory && metadata.len() > limits.max_file_bytes {
            return Err(LiveError::Source(SyncError::FileSizeLimitExceeded));
        }
        let remaining_bytes = limits.max_total_bytes - total_bytes;
        if !directory && metadata.len() > remaining_bytes {
            return Err(LiveError::Source(SyncError::TotalBytesLimitExceeded));
        }
        let remaining =
            usize::try_from(limits.max_entries - inspected).map_err(|_| LiveError::Resource)?;
        let mut authority_failure = None;
        let snapshot = sender::snapshot_selected(
            file,
            &name,
            directory,
            limits.max_file_bytes.min(remaining_bytes),
            remaining,
            &mut || {
                check().map_err(|error| {
                    authority_failure = Some(error);
                    sender::Error::Cancelled
                })
            },
        );
        if let Some(error) = authority_failure {
            return Err(LiveError::Source(error));
        }
        let sender::SourceSnapshot {
            version,
            inspected_entries,
        } = snapshot.map_err(source_failure)?;
        inspected = inspected
            .checked_add(u32::try_from(inspected_entries).map_err(|_| LiveError::Resource)?)
            .filter(|&count| count <= limits.max_entries)
            .ok_or(LiveError::Resource)?;
        total_bytes = total_bytes
            .checked_add(version.size())
            .filter(|&bytes| bytes <= limits.max_total_bytes)
            .ok_or(LiveError::Source(SyncError::TotalBytesLimitExceeded))?;
        let key = version.ledger_key();
        entries.insert(
            name,
            Entry {
                original,
                version,
                key,
            },
        );
    }
    check().map_err(LiveError::Source)?;
    Ok(entries)
}

fn source_failure(error: sender::Error) -> LiveError {
    match error {
        sender::Error::SourceChanged | sender::Error::Io(std::io::ErrorKind::NotFound) => {
            LiveError::Source(SyncError::SourceChanged)
        }
        sender::Error::Limits => LiveError::Resource,
        sender::Error::Name => LiveError::Source(SyncError::InvalidEntryName),
        sender::Error::Io(kind) => LiveError::Source(SyncError::Io(kind)),
        error => LiveError::Send(error),
    }
}

/// Classify without following links BEFORE opening any child. NONBLOCK closes
/// the regular-file-to-FIFO race; directory-only opens and identity comparison
/// reject replacement before a tree walk can begin.
fn open_entry(root: BorrowedFd<'_>, name: &str) -> Result<File, SyncError> {
    let stat = fs::statat(root, name, AtFlags::SYMLINK_NOFOLLOW).map_err(entry_error)?;
    let kind = FileType::from_raw_mode(stat.st_mode);
    if kind == FileType::Symlink {
        return Err(SyncError::SymlinkForbidden);
    }
    if !matches!(kind, FileType::RegularFile | FileType::Directory) {
        return Err(SyncError::UnsupportedEntryKind);
    }
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    if kind == FileType::Directory {
        flags |= OFlags::DIRECTORY;
    }
    let file = File::from(fs::openat(root, name, flags, Mode::empty()).map_err(entry_error)?);
    let metadata = file.metadata()?;
    if metadata.dev() != device(&stat)
        || metadata.ino() != stat.st_ino
        || metadata.is_dir() != (kind == FileType::Directory)
        || (!metadata.is_dir() && !metadata.is_file())
    {
        return Err(SyncError::SourceChanged);
    }
    Ok(file)
}

fn entry_error(error: crate::rx::io::Errno) -> SyncError {
    if error == crate::rx::io::Errno::NOENT {
        SyncError::SourceChanged
    } else if error == crate::rx::io::Errno::LOOP {
        SyncError::SymlinkForbidden
    } else {
        SyncError::from(error)
    }
}

#[cfg(target_os = "linux")]
fn device(stat: &fs::Stat) -> u64 {
    stat.st_dev
}
#[cfg(not(target_os = "linux"))]
#[allow(
    clippy::cast_sign_loss,
    reason = "match std's Unix device-number widening"
)]
fn device(stat: &fs::Stat) -> u64 {
    stat.st_dev as u64
}

#[cfg(test)]
mod tests;
