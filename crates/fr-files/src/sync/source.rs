//! Descriptor-relative sync reads with cancellation between bounded disk work.
use super::{FolderManifest, ManifestFile, SyncError, SyncLimits};
use crate::{
    receive::{DropDirectory, Error as ReceiveError, PublishedFile},
    rx::{
        MetadataExt,
        fd::{AsFd, BorrowedFd, OwnedFd},
        fs::{self, Mode, OFlags},
    },
};
use asupersync::net::atp::transport_common::{
    StagedEntryReceive, flat_merkle_root_from_digests, hex_encode,
};
use std::{
    collections::BTreeMap,
    fs::{File, Metadata},
    io::Read,
    path::Path,
};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Identity(u64, u64, u64, i64, i64, i64, i64);

fn identity(meta: &Metadata) -> Identity {
    Identity(
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

fn unchanged(file: &File, original: &Identity) -> Result<(), SyncError> {
    if identity(&file.metadata()?) != *original {
        return Err(SyncError::SourceChanged);
    }
    Ok(())
}

pub(super) fn open_root(path: &Path) -> Result<OwnedFd, SyncError> {
    if !path.is_absolute() {
        return Err(SyncError::InvalidRoot);
    }
    crate::receive::open_root(path).map_err(|error| match error {
        ReceiveError::InvalidRoot => SyncError::InvalidRoot,
        ReceiveError::UnsafeRoot => SyncError::UnsafeRoot,
        error => SyncError::Receive(error),
    })
}

/// The path and receive owner must select the same directory at the start of
/// the round. All subsequent reads use the owner's original descriptor.
pub(super) fn check_root(path: &Path, selected: &DropDirectory) -> Result<(), SyncError> {
    let current = fs::fstat(open_root(path)?.as_fd())?;
    let original = fs::fstat(selected.root_fd())?;
    if current.st_dev != original.st_dev || current.st_ino != original.st_ino {
        return Err(SyncError::RootChanged);
    }
    Ok(())
}

fn open_regular(root: BorrowedFd<'_>, name: &str) -> Result<File, SyncError> {
    // A regular entry may be replaced with a FIFO between enumeration and
    // open. NONBLOCK prevents waiting for a writer before the type check.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd = fs::openat(root, name, flags, Mode::empty()).map_err(|errno| {
        if errno == crate::rx::io::Errno::LOOP {
            SyncError::SymlinkForbidden
        } else if errno == crate::rx::io::Errno::NOENT {
            // All callers use a name already enumerated or manifested under
            // this retained root. Losing that entry invalidates the snapshot;
            // root/enumeration I/O errors keep their own terminal error kind.
            SyncError::SourceChanged
        } else {
            SyncError::from(errno)
        }
    })?;
    let file = File::from(fd);
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(SyncError::SubdirectoryUnsupported);
    }
    if !meta.is_file() {
        return Err(SyncError::UnsupportedEntryKind);
    }
    Ok(file)
}

/// Retain precisely the file version inspected by this bounded scan. The
/// transfer worker hashes it again and compares the expected digest before
/// offering it, so edits between scan and send cannot advance the wrong ledger.
pub(super) fn select(root: BorrowedFd<'_>, manifest: &ManifestFile) -> Result<File, SyncError> {
    let file = open_regular(root, &manifest.name)?;
    unchanged(&file, &manifest.original)?;
    Ok(file)
}

pub(super) fn scan<F>(
    root: BorrowedFd<'_>,
    revision: u64,
    limits: SyncLimits,
    check: &mut F,
) -> Result<FolderManifest, SyncError>
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
        // Bound enumeration and allocation, including private staging names.
        // Checking after collection would permit arbitrary memory/work here.
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

    let mut entries = BTreeMap::new();
    let mut digests = Vec::with_capacity(names.len());
    let mut total_bytes = 0u64;
    let mut buffer = vec![0; limits.max_chunk_bytes];
    for name in names {
        check()?;
        let mut file = open_regular(root, &name)?;
        let meta = file.metadata()?;
        let original = identity(&meta);
        let size = meta.len();
        if size > limits.max_file_bytes {
            return Err(SyncError::FileSizeLimitExceeded);
        }
        let next_total = total_bytes
            .checked_add(size)
            .ok_or(SyncError::TotalBytesLimitExceeded)?;
        if next_total > limits.max_total_bytes {
            return Err(SyncError::TotalBytesLimitExceeded);
        }
        let mut hasher = StagedEntryReceive::new(name.clone().into());
        hasher.mark_created();
        let mut remaining = size;
        while remaining != 0 {
            check()?;
            let count = usize::try_from(remaining)
                .unwrap_or(buffer.len())
                .min(buffer.len());
            let read = file.read(&mut buffer[..count])?;
            check()?;
            if read == 0 {
                return Err(SyncError::SourceChanged);
            }
            remaining -= read as u64;
            hasher.update_with_chunk(&buffer[..read]);
        }
        check()?;
        // Reject growth and proc-style files whose metadata understates data.
        if file.read(&mut [0; 1])? != 0 {
            return Err(SyncError::SourceChanged);
        }
        unchanged(&file, &original)?;
        check()?;
        let (digest, _, _) = hasher.finalize(name.clone());
        let sha256_hex = hex_encode(&digest.content_sha256);
        let merkle_root_hex = flat_merkle_root_from_digests(std::slice::from_ref(&digest));
        digests.push(digest);
        total_bytes = next_total;
        entries.insert(
            name.clone(),
            ManifestFile {
                name,
                size,
                sha256_hex,
                merkle_root_hex,
                original,
            },
        );
    }
    check()?;
    Ok(FolderManifest {
        revision,
        entries,
        total_bytes,
        root_hash_hex: flat_merkle_root_from_digests(&digests),
        inspected_entries: inspected,
    })
}

pub(super) fn transfer<F>(
    root: BorrowedFd<'_>,
    destination: &DropDirectory,
    manifest: &ManifestFile,
    chunk_bytes: usize,
    check: &mut F,
) -> Result<PublishedFile, SyncError>
where
    F: FnMut() -> Result<(), SyncError>,
{
    check()?;
    let mut file = open_regular(root, &manifest.name)?;
    unchanged(&file, &manifest.original)?;
    check()?;
    let mut pending = destination.begin_manifest(
        &manifest.name,
        manifest.size,
        manifest.sha256_hex.clone(),
        manifest.merkle_root_hex.clone(),
    )?;
    let mut buffer = vec![0; chunk_bytes];
    let mut offset = 0;
    while offset < manifest.size {
        check()?;
        let count = usize::try_from(manifest.size - offset)
            .unwrap_or(chunk_bytes)
            .min(chunk_bytes);
        let read = file.read(&mut buffer[..count])?;
        check()?;
        if read == 0 {
            return Err(SyncError::SourceChanged);
        }
        pending.write_chunk(offset, &buffer[..read])?;
        offset += read as u64;
    }
    check()?;
    if file.read(&mut [0; 1])? != 0 {
        return Err(SyncError::SourceChanged);
    }
    unchanged(&file, &manifest.original)?;
    check()?;
    pending.verify()?;
    pending.publish_named_checked(|| {
        unchanged(&file, &manifest.original)?;
        check()
    })
}
