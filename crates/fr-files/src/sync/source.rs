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
    EntryDigest, StagedEntryReceive, flat_merkle_root_from_digests, hex_encode,
};
use std::{
    collections::BTreeMap,
    fs::{File, Metadata},
    io::Read,
    path::Path,
};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Identity(u64, u64, u64, i64, i64, i64, i64);

pub(super) fn identity(meta: &Metadata) -> Identity {
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

pub(super) fn unchanged(file: &File, original: &Identity) -> Result<(), SyncError> {
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

/// The exact ATP digests stay with the original bounded scan. Keeping the
/// content identifier as well as SHA-256 preserves ATP's canonical Merkle root.
pub(super) struct Scan {
    pub(super) manifest: FolderManifest,
    digests: Vec<EntryDigest>,
    pub(super) reused: bool,
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
    scan_with_previous(root, revision, limits, None, check).map(|scan| scan.manifest)
}

/// Rebuild the current manifest from one bounded enumeration. A prior digest
/// may avoid a content read, but never supplies the current set of names, size
/// accounting, authority, or an unbounded verification lifetime.
pub(super) fn scan_with_previous<F>(
    root: BorrowedFd<'_>,
    revision: u64,
    limits: SyncLimits,
    previous: Option<&Scan>,
    check: &mut F,
) -> Result<Scan, SyncError>
where
    F: FnMut() -> Result<(), SyncError>,
{
    let names = entry_names(root, limits, check)?;
    let mut entries = BTreeMap::new();
    let mut digests = Vec::with_capacity(names.values.len());
    let mut total_bytes = 0;
    let mut buffer = vec![0; limits.max_chunk_bytes];
    let mut reused = false;
    for name in names.values {
        check()?;
        let (entry, digest) = if let Some(cached) = reusable(root, &name, previous, check)? {
            total_bytes = admit_size(total_bytes, cached.0.size, limits)?;
            reused = true;
            cached
        } else {
            let file = open_regular(root, &name)?;
            let metadata = file.metadata()?;
            total_bytes = admit_size(total_bytes, metadata.len(), limits)?;
            hash_file(file, name, &metadata, &mut buffer, check)?
        };
        digests.push(digest);
        entries.insert(entry.name.clone(), entry);
    }
    check()?;
    Ok(Scan {
        manifest: FolderManifest {
            revision,
            entries,
            total_bytes,
            root_hash_hex: flat_merkle_root_from_digests(&digests),
            inspected_entries: names.inspected,
        },
        digests,
        reused,
    })
}

pub(super) struct Names {
    pub(super) values: Vec<String>,
    pub(super) inspected: u32,
}

pub(super) fn entry_names<F>(
    root: BorrowedFd<'_>,
    limits: SyncLimits,
    check: &mut F,
) -> Result<Names, SyncError>
where
    F: FnMut() -> Result<(), SyncError>,
{
    check()?;
    let mut directory = fs::Dir::read_from(root)?;
    let mut names = Names {
        values: Vec::new(),
        inspected: 0,
    };
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
        // Count private staging entries before skipping their content.
        if names.inspected == limits.max_entries {
            return Err(SyncError::EntryLimitExceeded);
        }
        names.inspected += 1;
        if name.starts_with(super::STAGING_PREFIX) {
            continue;
        }
        crate::receive::validate_name(name).map_err(|_| SyncError::InvalidEntryName)?;
        names.values.push(name.to_owned());
    }
    order_names(&mut names.values)?;
    Ok(names)
}

fn order_names(names: &mut [String]) -> Result<(), SyncError> {
    names.sort();
    // A changing directory iterator can repeat an entry. Never let the ordered
    // digest vector and keyed manifest disagree about which version a name owns.
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(SyncError::SourceChanged);
    }
    Ok(())
}

fn admit_size(total: u64, size: u64, limits: SyncLimits) -> Result<u64, SyncError> {
    if size > limits.max_file_bytes {
        return Err(SyncError::FileSizeLimitExceeded);
    }
    let next = total
        .checked_add(size)
        .ok_or(SyncError::TotalBytesLimitExceeded)?;
    if next > limits.max_total_bytes {
        return Err(SyncError::TotalBytesLimitExceeded);
    }
    Ok(next)
}

fn reusable<F>(
    root: BorrowedFd<'_>,
    name: &str,
    previous: Option<&Scan>,
    check: &mut F,
) -> Result<Option<(ManifestFile, EntryDigest)>, SyncError>
where
    F: FnMut() -> Result<(), SyncError>,
{
    let Some(previous) = previous else {
        return Ok(None);
    };
    let Some(expected) = previous.manifest.entries.get(name) else {
        return Ok(None);
    };
    let Ok(first) = select(root, expected) else {
        return Ok(None);
    };
    let metadata = first.metadata()?;
    // Empty/proc-style files and coarse timestamps retain the content/EOF probe.
    if metadata.len() == 0
        || metadata.mtime() <= 0
        || metadata.ctime() <= 0
        || !(1..1_000_000_000).contains(&metadata.mtime_nsec())
        || !(1..1_000_000_000).contains(&metadata.ctime_nsec())
    {
        return Ok(None);
    }
    check()?;
    // Keep the first descriptor through the second checked open. A replaced
    // pathname cannot recycle its inode while that original descriptor lives.
    let Ok(second) = select(root, expected) else {
        return Ok(None);
    };
    check()?;
    if unchanged(&first, &expected.original).is_err()
        || unchanged(&second, &expected.original).is_err()
    {
        return Ok(None);
    }
    let index = previous
        .digests
        .binary_search_by(|digest| digest.rel_path.as_str().cmp(name))
        .expect("scan digests and manifest have the same ordered names");
    Ok(Some((expected.clone(), previous.digests[index].clone())))
}

fn hash_file<F>(
    mut file: File,
    name: String,
    metadata: &Metadata,
    buffer: &mut [u8],
    check: &mut F,
) -> Result<(ManifestFile, EntryDigest), SyncError>
where
    F: FnMut() -> Result<(), SyncError>,
{
    let original = identity(metadata);
    let size = metadata.len();
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
    let entry = ManifestFile {
        name,
        size,
        sha256_hex: hex_encode(&digest.content_sha256),
        merkle_root_hex: flat_merkle_root_from_digests(std::slice::from_ref(&digest)),
        original,
    };
    Ok((entry, digest))
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

#[cfg(test)]
mod tests {
    use super::{SyncError, order_names};

    #[test]
    fn repeated_directory_entries_refuse_before_manifest_or_digest_construction() {
        let mut repeated = ["later", "first", "later"].map(str::to_owned);
        assert_eq!(order_names(&mut repeated), Err(SyncError::SourceChanged));
        let mut unique = ["later", "first"].map(str::to_owned);
        assert_eq!(order_names(&mut unique), Ok(()));
        assert_eq!(unique, ["first", "later"]);
    }
}
