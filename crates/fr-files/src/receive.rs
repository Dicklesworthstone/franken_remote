//! Handle-relative, no-overwrite publication of bounded ATP objects on Linux.
//!
//! Directory descriptors, not re-resolved path strings, anchor every operation.
//! The selected desktop user and the OS remain trusted (SECURITY.md); other
//! users must not be able to mutate the drop directory. Peer metadata never
//! supplies an absolute path, an executable mode, or a symlink to materialize.
mod conflict;
pub use conflict::{ConflictPolicy, PublishedFile};
use conflict::{MAX_CONFLICT_ATTEMPTS, conflict_name};

use asupersync::atp::{
    object::{ContentId, ObjectId},
    safety::validate_portable_path_component,
};
use asupersync::net::atp::transport_common::{
    StagedEntryReceive, flat_merkle_root_from_digests, hex_encode,
};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, Mode, OFlags, RenameFlags},
    process::geteuid,
};
use std::{
    fmt,
    fs::File,
    io::{self, Write},
    path::{Component, Path},
    sync::{Arc, Mutex},
};

/// A record is rejected before disk I/O when it exceeds this bound.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
const STAGING_PREFIX: &str = ".fr-part-";

/// Aggregate active reservations across every clone of a selected directory.
/// This is not a quota on completed files; available disk space remains an OS
/// constraint and a disk-full write is a terminal, unpublished transfer failure.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_reserved_bytes: u64,
    pub max_transfers: u32,
}

/// Sanitized failures: never contain a peer filename, payload, path, or digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidLimits,
    InvalidRoot,
    UnsafeRoot,
    InvalidName,
    Quota,
    InvalidChunk,
    Incomplete,
    Integrity,
    Conflict,
    Retired,
    Io(io::ErrorKind),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "file-receive: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value.kind())
    }
}
impl From<rustix::io::Errno> for Error {
    fn from(value: rustix::io::Errno) -> Self {
        io::Error::from(value).into()
    }
}

#[derive(Default)]
struct Reserved {
    bytes: u64,
    transfers: u32,
}
struct Root {
    fd: OwnedFd,
    limits: Limits,
    reserved: Mutex<Reserved>,
}

/// A locally selected directory identity, never constructible from a wire path.
/// Renaming/replacing the original path does not redirect writes to another
/// directory. Clones share both its descriptor and its aggregate reservations.
#[derive(Clone)]
pub struct DropDirectory(Arc<Root>, ConflictPolicy);
impl fmt::Debug for DropDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DropDirectory([local directory])")
    }
}
impl DropDirectory {
    /// Open an existing absolute directory without following any symlink.
    /// Its final component must be owned by this user and not writable by
    /// another user/group. Parent directories need not be private: they are
    /// traversed once with NOFOLLOW and all later operations use the pinned fd.
    pub fn open(path: &Path, limits: Limits) -> Result<Self, Error> {
        if limits.max_file_bytes == 0
            || limits.max_reserved_bytes < limits.max_file_bytes
            || limits.max_transfers == 0
        {
            return Err(Error::InvalidLimits);
        }
        if !path.is_absolute() {
            return Err(Error::InvalidRoot);
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut fd = fs::open("/", flags, Mode::empty())?;
        let mut normal = false;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    fd = fs::openat(&fd, name, flags, Mode::empty())?;
                    normal = true;
                }
                _ => return Err(Error::InvalidRoot),
            }
        }
        if !normal {
            return Err(Error::InvalidRoot);
        }
        let stat = fs::fstat(&fd)?;
        if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
            return Err(Error::UnsafeRoot);
        }
        Ok(Self(
            Arc::new(Root {
                fd,
                limits,
                reserved: Mutex::new(Reserved::default()),
            }),
            ConflictPolicy::Reject,
        ))
    }

    /// Choose conflict handling locally before attaching this directory to a
    /// session. Clones still share the same pinned identity and aggregate quota;
    /// choosing a different policy never creates a fresh reservation budget.
    #[must_use]
    pub fn with_conflict_policy(mut self, policy: ConflictPolicy) -> Self {
        self.1 = policy;
        self
    }

    /// Reserve a declared object before creating any temporary file. The caller
    /// must already hold separately approved file-receive permission. `expected`
    /// is ATP's domain-separated `ContentId`, NOT a plain SHA-256 file digest.
    pub fn begin(&self, name: &str, size: u64, expected: ContentId) -> Result<PendingFile, Error> {
        self.begin_verified(name, size, Expected::Content(expected))
    }

    /// Reserve an object whose expected integrity is defined by an ATP manifest
    /// SHA-256 and flat Merkle root.
    pub fn begin_manifest(
        &self,
        name: &str,
        size: u64,
        sha256_hex: String,
        merkle_root_hex: String,
    ) -> Result<PendingFile, Error> {
        self.begin_verified(
            name,
            size,
            Expected::Manifest {
                sha256_hex,
                merkle_root_hex,
            },
        )
    }

    pub(crate) fn begin_verified(
        &self,
        name: &str,
        size: u64,
        expected: Expected,
    ) -> Result<PendingFile, Error> {
        if name.len() > 255
            || name.starts_with(STAGING_PREFIX)
            || validate_portable_path_component(name).is_err()
        {
            return Err(Error::InvalidName);
        }
        let reservation = Reservation::new(self.0.clone(), size)?;
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Io(io::ErrorKind::Other))?;
        let suffix = u128::from_be_bytes(random);
        let staging = format!("{STAGING_PREFIX}{suffix:032x}");
        let fd = fs::openat(
            &self.0.fd,
            staging.as_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        // ATP's streaming verifier performs no filesystem access. Its private,
        // relative staging label is bookkeeping; our pinned fd owns all I/O.
        let mut hasher = StagedEntryReceive::new(staging.clone().into());
        hasher.mark_created();
        Ok(PendingFile {
            reservation,
            file: fd.into(),
            staging,
            destination: name.to_owned(),
            conflict_policy: self.1,
            nonce: suffix,
            size,
            received: 0,
            expected,
            hasher: Some(hasher),
            state: State::Receiving,
        })
    }
}

/// Select one existing ATP integrity contract, never reinterpret a plain hash
/// as a domain-separated content id. Manifest values are validated by `atp`.
pub(crate) enum Expected {
    Content(ContentId),
    Manifest {
        sha256_hex: String,
        merkle_root_hex: String,
    },
}

struct Reservation {
    root: Arc<Root>,
    size: u64,
    // Failed cleanup keeps the charge: an orphan must not turn into a quota
    // bypass. The directory must be inspected locally before reuse/reset.
    release: bool,
}
impl Reservation {
    fn new(root: Arc<Root>, size: u64) -> Result<Self, Error> {
        {
            let mut held = root.reserved.lock().map_err(|_| Error::Retired)?;
            let total = held.bytes.checked_add(size).ok_or(Error::Quota)?;
            if size > root.limits.max_file_bytes
                || total > root.limits.max_reserved_bytes
                || held.transfers >= root.limits.max_transfers
            {
                return Err(Error::Quota);
            }
            held.bytes = total;
            held.transfers += 1;
        }
        Ok(Self {
            root,
            size,
            release: true,
        })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.release {
            // Poison is fail-closed: never resurrect the directory's budget.
            if let Ok(mut held) = self.root.reserved.lock() {
                held.bytes -= self.size;
                held.transfers -= 1;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Receiving,
    Verified,
    Failed,
    Published,
    Cancelled,
}

/// One unpublished regular file. Dropping it removes ONLY its own staging name.
/// Failure is terminal; a partially failed write cannot be retried as if it had
/// no effects. No payload is accumulated in memory and offsets are sequential.
/// Process death may leave private staging files for local cleanup, never a
/// partially written destination or an automatically resumed authority grant.
pub struct PendingFile {
    reservation: Reservation,
    file: File,
    staging: String,
    destination: String,
    conflict_policy: ConflictPolicy,
    nonce: u128,
    size: u64,
    received: u64,
    expected: Expected,
    hasher: Option<StagedEntryReceive>,
    state: State,
}
impl fmt::Debug for PendingFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingFile([private staged object])")
    }
}

/// Publication has happened in BOTH cases. Durability uncertainty is not a
/// refusal and MUST NOT cause an automatic retry or overwrite of the final name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    Durable,
    DurabilityUnknown,
}

impl PendingFile {
    pub fn received_bytes(&self) -> u64 {
        self.received
    }

    /// Append one bounded ATP object slice. An invalid record retires this
    /// transfer rather than allowing an attacker to probe/reuse a partial state.
    pub fn write_chunk(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Error> {
        if self.state != State::Receiving {
            return Err(Error::Retired);
        }
        self.state = State::Failed;
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(Error::InvalidChunk)?;
        if bytes.is_empty()
            || bytes.len() > MAX_CHUNK_BYTES
            || offset != self.received
            || end > self.size
        {
            return Err(Error::InvalidChunk);
        }
        self.file.write_all(bytes)?;
        self.hasher
            .as_mut()
            .ok_or(Error::Retired)?
            .update_with_chunk(bytes);
        self.received = end;
        self.state = State::Receiving;
        Ok(())
    }

    /// Check all bytes and durably stage them, but do NOT publish yet. The
    /// separate publication step lets the session recheck its original authority
    /// after slow filesystem work, immediately before the external effect.
    pub fn verify(&mut self) -> Result<(), Error> {
        if self.state != State::Receiving {
            return Err(Error::Retired);
        }
        self.state = State::Failed;
        if self.received != self.size {
            return Err(Error::Incomplete);
        }
        let (digest, _, _) = self
            .hasher
            .take()
            .ok_or(Error::Retired)?
            .finalize(self.destination.clone());
        let valid = match &self.expected {
            Expected::Content(id) => digest.content_id == ObjectId::content(id.clone()),
            Expected::Manifest {
                sha256_hex,
                merkle_root_hex,
            } => {
                hex_encode(&digest.content_sha256) == *sha256_hex
                    && flat_merkle_root_from_digests(std::slice::from_ref(&digest))
                        == *merkle_root_hex
            }
        };
        if !valid || digest.size != self.size {
            return Err(Error::Integrity);
        }
        self.file.sync_all()?;
        self.state = State::Verified;
        Ok(())
    }

    /// Atomically publish a previously verified object. Never replaces an
    /// existing file, directory, or symlink, even if it appeared after admission.
    /// Call only after a fresh original-controller and file-permission check.
    pub fn publish(self) -> Result<Publication, Error> {
        self.publish_named().map(|result| result.publication())
    }

    /// The same atomic publication with its actual local basename retained for
    /// the receiving UI. Names are intentionally absent from `Debug` and wire
    /// diagnostics. A keep-both policy does not weaken the original ATP integrity
    /// check: verify the advertised manifest BEFORE choosing a conflict name.
    pub fn publish_named(mut self) -> Result<PublishedFile, Error> {
        if self.state != State::Verified {
            return Err(Error::Retired);
        }
        self.state = State::Failed;
        let original = self.destination.clone();
        let mut attempt = 0;
        loop {
            match fs::renameat_with(
                &self.reservation.root.fd,
                self.staging.as_str(),
                &self.reservation.root.fd,
                self.destination.as_str(),
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => break,
                Err(rustix::io::Errno::EXIST)
                    if self.conflict_policy == ConflictPolicy::KeepBoth
                        && attempt < MAX_CONFLICT_ATTEMPTS =>
                {
                    self.destination = conflict_name(&original, self.nonce, attempt);
                    attempt += 1;
                }
                Err(rustix::io::Errno::EXIST) => return Err(Error::Conflict),
                Err(error) => return Err(error.into()),
            }
        }
        // After rename succeeds, cleanup MUST NOT unlink this destination even
        // if the directory sync fails. Publication uncertainty is not rollback.
        self.state = State::Published;
        let publication = if fs::fsync(&self.reservation.root.fd).is_ok() {
            Publication::Durable
        } else {
            Publication::DurabilityUnknown
        };
        Ok(PublishedFile {
            name: std::mem::take(&mut self.destination),
            publication,
            renamed: attempt != 0,
        })
    }

    /// Explicit cancellation reports cleanup errors. Drop is a best-effort
    /// backstop; neither path touches a published or conflicting destination.
    pub fn cancel(mut self) -> Result<(), Error> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<(), Error> {
        if matches!(self.state, State::Published | State::Cancelled) {
            return Ok(());
        }
        match fs::unlinkat(
            &self.reservation.root.fd,
            self.staging.as_str(),
            AtFlags::empty(),
        ) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {
                self.state = State::Cancelled;
                self.reservation.release = true;
                Ok(())
            }
            Err(error) => {
                self.reservation.release = false;
                Err(error.into())
            }
        }
    }
}
impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
