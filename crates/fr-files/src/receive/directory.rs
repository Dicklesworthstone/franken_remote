//! Portable ATP file trees, staged beneath one private directory and published
//! with one no-replace rename. No peer path is resolved from the process cwd.
use super::{Error, MAX_CHUNK_BYTES, Publication, Reservation, STAGING_PREFIX, State};
use asupersync::net::atp::{
    transport_common::{StagedEntryReceive, flat_merkle_root_from_digests, hex_encode},
    transport_tcp::ManifestEntry,
};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, Mode, OFlags, RenameFlags},
};
use std::{collections::BTreeMap, fs::File, io::Write};

/// Bounds include implicit directories, not just content bytes. Empty files
/// cannot evade descriptor, inode, manifest, or cleanup-work admission.
pub const MAX_TREE_ENTRIES: usize = 64;
pub const MAX_TREE_NODES: usize = 128;
pub const MAX_TREE_DEPTH: usize = 8;
pub const MAX_TREE_PATH_BYTES: usize = 1024;
pub const MAX_TREE_NAMES_BYTES: usize = 16 * 1024;

/// A fully validated portable tree. Metadata, links, packed members, deltas,
/// and empty non-root directories are outside this content-only ATP profile.
/// Paths and hashes are deliberately omitted from Debug and errors.
pub struct DirectoryManifest {
    pub(super) name: String,
    pub(super) size: u64,
    entries: Vec<ManifestEntry>,
    root: String,
}
impl std::fmt::Debug for DirectoryManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DirectoryManifest([bounded portable tree])")
    }
}
impl DirectoryManifest {
    pub fn new(
        name: String,
        size: u64,
        entries: Vec<ManifestEntry>,
        root: String,
    ) -> Result<Self, Error> {
        super::validate_name(&name)?;
        if entries.len() > MAX_TREE_ENTRIES || !lower_hash(&root) {
            return Err(Error::InvalidLimits);
        }
        // Include every implicit parent, rejecting file/directory collisions
        // and case aliases rather than depending on target filesystem rules.
        let mut nodes = BTreeMap::<String, (String, bool)>::new();
        let mut total = 0u64;
        let mut names = name.len();
        for (index, entry) in entries.iter().enumerate() {
            if entry.index as usize != index
                || entry.metadata.is_some()
                || !entry.members.is_empty()
                || !lower_hash(&entry.sha256_hex)
            {
                return Err(Error::InvalidName);
            }
            let parts = path_components(&entry.rel_path)?;
            names = names
                .checked_add(entry.rel_path.len())
                .ok_or(Error::Quota)?;
            if names > MAX_TREE_NAMES_BYTES {
                return Err(Error::Quota);
            }
            total = total.checked_add(entry.size).ok_or(Error::Quota)?;
            let mut path = String::new();
            for (n, part) in parts.iter().enumerate() {
                if n != 0 {
                    path.push('/');
                }
                path.push_str(part);
                let file = n + 1 == parts.len();
                let key = path.to_lowercase();
                if let Some((existing, existing_file)) = nodes.get(&key) {
                    if existing != &path || file || *existing_file {
                        return Err(Error::InvalidName);
                    }
                } else {
                    nodes.insert(key, (path.clone(), file));
                    if nodes.len() > MAX_TREE_NODES {
                        return Err(Error::Quota);
                    }
                }
            }
        }
        if total != size {
            return Err(Error::Integrity);
        }
        Ok(Self {
            name,
            size,
            entries,
            root,
        })
    }
    pub fn file_count(&self) -> usize {
        self.entries.len()
    }
    pub fn total_bytes(&self) -> u64 {
        self.size
    }
    pub(crate) fn metadata_bytes(&self) -> u64 {
        // Account hashes, names, indices and fixed per-entry work in the lane's
        // existing rate bucket. Structural admission bounds allocations too.
        self.entries
            .iter()
            .map(|e| e.rel_path.len() as u64 + 128)
            .sum()
    }
}
pub(crate) fn path_components(path: &str) -> Result<Vec<&str>, Error> {
    if path.is_empty() || path.len() > MAX_TREE_PATH_BYTES {
        return Err(Error::InvalidName);
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() > MAX_TREE_DEPTH {
        return Err(Error::InvalidName);
    }
    for part in &parts {
        super::validate_name(part)?;
    }
    Ok(parts)
}
fn lower_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
struct Directory {
    fd: OwnedFd,
    parent: usize,
    name: String,
}
struct Entry {
    file: File,
    parent: usize,
    name: String,
    path: String,
    size: u64,
    received: u64,
    sha: String,
    hasher: Option<StagedEntryReceive>,
}
/// All descriptors and exact created names are owned until cleanup/publication.
/// Cleanup never recursively walks peer-controlled or newly inserted paths.
pub struct PendingDirectory {
    reservation: Reservation,
    directories: Vec<Directory>,
    entries: Vec<Entry>,
    destination: String,
    root: String,
    received: u64,
    state: State,
}
impl std::fmt::Debug for PendingDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingDirectory([private staged tree])")
    }
}
impl super::DropDirectory {
    pub fn begin_directory(&self, manifest: DirectoryManifest) -> Result<PendingDirectory, Error> {
        let mut reservation = Reservation::new(self.0.clone(), manifest.size)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Io(std::io::ErrorKind::Other))?;
        let name = format!("{STAGING_PREFIX}{:032x}", u128::from_be_bytes(random));
        fs::mkdirat(&self.0.fd, name.as_str(), Mode::from_raw_mode(0o700))?;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = match fs::openat(&self.0.fd, name.as_str(), flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                reservation.release =
                    fs::unlinkat(&self.0.fd, name.as_str(), AtFlags::REMOVEDIR).is_ok();
                return Err(error.into());
            }
        };
        let mut pending = PendingDirectory {
            reservation,
            directories: vec![Directory {
                fd,
                parent: 0,
                name,
            }],
            entries: Vec::new(),
            destination: manifest.name,
            root: manifest.root,
            received: 0,
            state: State::Receiving,
        };
        let mut paths = BTreeMap::new();
        paths.insert(String::new(), 0usize);
        for entry in manifest.entries {
            let parts = path_components(&entry.rel_path)?;
            let mut parent = 0usize;
            let mut path = String::new();
            for part in &parts[..parts.len() - 1] {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(part);
                if let Some(index) = paths.get(&path) {
                    parent = *index;
                    continue;
                }
                fs::mkdirat(
                    &pending.directories[parent].fd,
                    *part,
                    Mode::from_raw_mode(0o700),
                )?;
                let fd = match fs::openat(
                    &pending.directories[parent].fd,
                    *part,
                    flags,
                    Mode::empty(),
                ) {
                    Ok(fd) => fd,
                    Err(error) => {
                        // This exact just-created child is ours. Unknown content
                        // is never removed; failed cleanup keeps the reservation.
                        let _ = fs::unlinkat(
                            &pending.directories[parent].fd,
                            *part,
                            AtFlags::REMOVEDIR,
                        );
                        return Err(error.into());
                    }
                };
                let index = pending.directories.len();
                pending.directories.push(Directory {
                    fd,
                    parent,
                    name: (*part).to_owned(),
                });
                paths.insert(path.clone(), index);
                parent = index;
            }
            let name = parts[parts.len() - 1].to_owned();
            let file = fs::openat(
                &pending.directories[parent].fd,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?
            .into();
            let mut hasher = StagedEntryReceive::new(entry.rel_path.clone().into());
            hasher.mark_created();
            pending.entries.push(Entry {
                file,
                parent,
                name,
                path: entry.rel_path,
                size: entry.size,
                received: 0,
                sha: entry.sha256_hex,
                hasher: Some(hasher),
            });
        }
        Ok(pending)
    }
}
impl PendingDirectory {
    pub fn received_bytes(&self) -> u64 {
        self.received
    }
    pub fn write_entry(&mut self, index: u32, offset: u64, bytes: &[u8]) -> Result<(), Error> {
        if self.state != State::Receiving {
            return Err(Error::Retired);
        }
        self.state = State::Failed;
        let entry = self
            .entries
            .get_mut(index as usize)
            .ok_or(Error::InvalidChunk)?;
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(Error::InvalidChunk)?;
        if bytes.is_empty()
            || bytes.len() > MAX_CHUNK_BYTES
            || offset != entry.received
            || end > entry.size
        {
            return Err(Error::InvalidChunk);
        }
        entry.file.write_all(bytes)?;
        entry
            .hasher
            .as_mut()
            .ok_or(Error::Retired)?
            .update_with_chunk(bytes);
        entry.received = end;
        self.received = self
            .received
            .checked_add(bytes.len() as u64)
            .ok_or(Error::Quota)?;
        self.state = State::Receiving;
        Ok(())
    }
    pub fn verify(&mut self) -> Result<(), Error> {
        if self.state != State::Receiving {
            return Err(Error::Retired);
        }
        self.state = State::Failed;
        let mut digests = Vec::with_capacity(self.entries.len());
        for entry in &mut self.entries {
            if entry.received != entry.size {
                return Err(Error::Incomplete);
            }
            let (digest, _, _) = entry
                .hasher
                .take()
                .ok_or(Error::Retired)?
                .finalize(entry.path.clone());
            if digest.size != entry.size || hex_encode(&digest.content_sha256) != entry.sha {
                return Err(Error::Integrity);
            }
            entry.file.sync_all()?;
            digests.push(digest);
        }
        if flat_merkle_root_from_digests(&digests) != self.root {
            return Err(Error::Integrity);
        }
        for directory in self.directories.iter().rev() {
            fs::fsync(&directory.fd)?;
        }
        self.state = State::Verified;
        Ok(())
    }
    pub fn publish(mut self) -> Result<Publication, Error> {
        if self.state != State::Verified {
            return Err(Error::Retired);
        }
        self.state = State::Failed;
        match fs::renameat_with(
            &self.reservation.root.fd,
            self.directories[0].name.as_str(),
            &self.reservation.root.fd,
            self.destination.as_str(),
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => (),
            Err(rustix::io::Errno::EXIST) => return Err(Error::Conflict),
            Err(error) => return Err(error.into()),
        }
        self.state = State::Published;
        Ok(if fs::fsync(&self.reservation.root.fd).is_ok() {
            Publication::Durable
        } else {
            Publication::DurabilityUnknown
        })
    }
    pub fn cancel(mut self) -> Result<(), Error> {
        self.cleanup()
    }
    fn cleanup(&mut self) -> Result<(), Error> {
        if matches!(self.state, State::Published | State::Cancelled) {
            return Ok(());
        }
        self.reservation.release = false;
        // Do not short circuit: reclaim every exact owned entry possible after
        // a partial failure. The root staying nonempty retains the entire charge.
        let mut failure = None;
        for entry in &self.entries {
            if let Err(error) = fs::unlinkat(
                &self.directories[entry.parent].fd,
                entry.name.as_str(),
                AtFlags::empty(),
            ) && error != rustix::io::Errno::NOENT
            {
                failure = Some(error);
            }
        }
        for directory in self.directories.iter().skip(1).rev() {
            if let Err(error) = fs::unlinkat(
                &self.directories[directory.parent].fd,
                directory.name.as_str(),
                AtFlags::REMOVEDIR,
            ) && error != rustix::io::Errno::NOENT
            {
                failure = Some(error);
            }
        }
        match fs::unlinkat(
            &self.reservation.root.fd,
            self.directories[0].name.as_str(),
            AtFlags::REMOVEDIR,
        ) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => (),
            Err(error) => failure = Some(error),
        }
        if let Some(error) = failure {
            return Err(error.into());
        }
        self.reservation.release = true;
        self.state = State::Cancelled;
        Ok(())
    }
}
impl Drop for PendingDirectory {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
