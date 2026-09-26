//! Explicit file sending from the CONTROLLING native viewer into the host's
//! operator-approved drop directory (`fr connect --control --send PATH` into
//! `frd run --input-agent PATH --files DIR`), plan §15.6.
//!
//! This module holds only local decisions and content-free status. The lane is
//! the existing one-use `native-file-receive` attachment with `file-channel-scope`
//! v1 (both OPTIONAL capabilities), opened only under the controller's ACTIVE
//! input attachment. Bytes travel the existing ATP full-object profile through
//! `fr_files` (bounded disk worker, staging file, no-replace atomic rename). The
//! host's receipt checks the controller's input lease before every write and
//! immediately before publication; a revoked or expired lease fences the
//! transfer and its private staging file is removed, never published.
//!
//! Stated limits of this slice: viewer to host only; one explicit selection
//! per connection, sent in order, stopping at the first failure (existing
//! batch semantics); never together with the clipboard lane (both setups share
//! one serialized control-route handshake, see `Absence::WithClipboard`);
//! no resumption, directories, downloads, synchronization or picker UI. File
//! names and contents never appear in `Debug`, errors or status.
use fr_files::{
    receive::{DropDirectory, Limits as DirectoryLimits},
    sender::{self, batch},
    session::{Permission, Policy as SessionPolicy},
};
use rustix::fs::{Mode, OFlags};
use std::{
    fmt,
    fs::File,
    io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

pub use fr_files::receive::Publication;
pub use fr_files::sender::batch::Report;

/// At most this many `--send` selections per connection (the batch owner's
/// own bound is 32; the command line stays well inside its argument limit).
pub const MAX_SENDS: usize = 8;
/// One absolute budget for the whole selection: queueing, hashing, sending,
/// host proof and source cleanup. Lease renewal never extends it.
pub const SEND_LIFETIME: Duration = Duration::from_mins(30);
/// The one-use attachment/ticket exchange deadline (the API's maximum).
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(2);
/// Admitted transfer attempts per controlled session (failed ones count too).
pub const MAX_SESSION_FILES: u32 = 64;
/// Default per-file and per-session byte limits when the operator gives none.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_MAX_SESSION_BYTES: u64 = 1024 * 1024 * 1024;

/// Operator-configured receive limits. Both are enforced before any byte of
/// an offered file is written: `max_file_bytes` by the directory reservation
/// (before the staging file exists), `max_session_bytes` by the session's
/// non-refundable cumulative declared bytes (cancelled/refused count too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_session_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_session_bytes: DEFAULT_MAX_SESSION_BYTES,
        }
    }
}

/// Why a `--files DIR` is refused. Carries no path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryRefusal {
    Relative,
    Missing,
    NotDirectory,
    /// The directory itself, or one of its parents, is a symbolic link.
    Symlink,
    NotOwned,
    WritableByOthers,
    Limits,
    Unavailable,
}
impl DirectoryRefusal {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Relative => "files_directory_relative",
            Self::Missing => "files_directory_missing",
            Self::NotDirectory => "files_directory_not_directory",
            Self::Symlink => "files_directory_symlink",
            Self::NotOwned => "files_directory_not_owned",
            Self::WritableByOthers => "files_directory_writable_by_others",
            Self::Limits => "files_limits_invalid",
            Self::Unavailable => "files_directory_unavailable",
        }
    }
}

/// The operator's receive directory, pinned by descriptor at startup: renaming
/// or replacing the path later never redirects writes. Clones share the pinned
/// descriptor and its active-reservation budget (one staged file at a time).
#[derive(Clone)]
pub struct Directory {
    directory: DropDirectory,
    limits: Limits,
}
impl fmt::Debug for Directory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the path.
        f.debug_struct("FileDropDirectory")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}
impl Directory {
    /// Accept only an existing absolute directory, owned by this effective
    /// user, not group/other-writable, reached without any symbolic link.
    /// The typed classification uses `lstat`; `DropDirectory::open` then
    /// re-checks ownership and mode on the pinned no-follow descriptor, so a
    /// swap between the two is refused rather than trusted.
    pub fn open(path: &Path, limits: Limits) -> Result<Self, DirectoryRefusal> {
        if limits.max_file_bytes == 0 || limits.max_session_bytes < limits.max_file_bytes {
            return Err(DirectoryRefusal::Limits);
        }
        if !path.is_absolute() {
            return Err(DirectoryRefusal::Relative);
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(DirectoryRefusal::Missing);
            }
            Err(_) => return Err(DirectoryRefusal::Unavailable),
        };
        if metadata.file_type().is_symlink()
            || path.ancestors().skip(1).any(|parent| {
                std::fs::symlink_metadata(parent).is_ok_and(|m| m.file_type().is_symlink())
            })
        {
            return Err(DirectoryRefusal::Symlink);
        }
        if !metadata.is_dir() {
            return Err(DirectoryRefusal::NotDirectory);
        }
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(DirectoryRefusal::NotOwned);
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(DirectoryRefusal::WritableByOthers);
        }
        let directory = DropDirectory::open(
            path,
            DirectoryLimits {
                max_file_bytes: limits.max_file_bytes,
                max_reserved_bytes: limits.max_file_bytes,
                // One controller, one active object: at most one staged file.
                max_transfers: 1,
            },
        )
        .map_err(|error| match error {
            fr_files::receive::Error::UnsafeRoot => DirectoryRefusal::NotOwned,
            fr_files::receive::Error::InvalidLimits => DirectoryRefusal::Limits,
            fr_files::receive::Error::InvalidRoot => DirectoryRefusal::Relative,
            _ => DirectoryRefusal::Unavailable,
        })?;
        Ok(Self { directory, limits })
    }
    pub const fn limits(&self) -> Limits {
        self.limits
    }
    /// One controlled session's receive configuration. Under the explicit
    /// `approval none` profile the operator's `--files` IS the local file
    /// permission for this OS share; it grants nothing by itself (the lane
    /// still needs the controller's own selection and its live input lease).
    pub(crate) fn configuration(&self) -> fr_files::quic::Configuration {
        fr_files::quic::Configuration {
            directory: self.directory.clone(),
            permission: Permission::new(true),
            policy: self.policy(),
            reply_lifetime: Duration::from_secs(1),
        }
    }
    fn policy(&self) -> SessionPolicy {
        SessionPolicy {
            max_session_bytes: self.limits.max_session_bytes,
            max_session_transfers: MAX_SESSION_FILES,
            ..SessionPolicy::conservative()
        }
    }
}

/// Why a `--send PATH` is refused before any network I/O. Index only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectReason {
    TooMany,
    Missing,
    Symlink,
    Directory,
    SpecialFile,
    NotPortable,
    DuplicateName,
    Unreadable,
    Changed,
}
impl SelectReason {
    pub const fn code(self) -> &'static str {
        match self {
            Self::TooMany => "send_too_many",
            Self::Missing => "send_missing",
            Self::Symlink => "send_symlink",
            Self::Directory => "send_directory",
            Self::SpecialFile => "send_special_file",
            Self::NotPortable => "send_name_not_portable",
            Self::DuplicateName => "send_duplicate_name",
            Self::Unreadable => "send_unreadable",
            Self::Changed => "send_changed",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectRefusal {
    /// Zero-based position among the `--send` arguments.
    pub index: usize,
    pub reason: SelectReason,
}

struct Selected {
    file: File,
    name: String,
    bytes: u64,
}
/// The user's explicit local selection: open regular-file descriptors and
/// their portable basenames (the host name is the final path component,
/// unchanged; a non-portable name is refused, never rewritten).
pub struct Selection {
    files: Vec<Selected>,
}
impl fmt::Debug for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSelection")
            .field("files", &self.files.len())
            .finish_non_exhaustive()
    }
}
fn portable(name: &str) -> bool {
    name.len() <= 255
        && !name.starts_with(".fr-part-")
        && asupersync::atp::safety::validate_portable_path_component(name).is_ok()
}
impl Selection {
    /// Classify every path by type with `lstat` BEFORE opening anything:
    /// symbolic links, directories and special files refuse by type. Then
    /// open with `O_NOFOLLOW | O_NONBLOCK` (a racing FIFO cannot block us)
    /// and require the opened descriptor to be the same regular file.
    /// Nothing is read here; hashing and reading happen on the sender's own
    /// bounded disk thread after control is granted.
    pub fn select(paths: &[PathBuf]) -> Result<Self, SelectRefusal> {
        if paths.is_empty() || paths.len() > MAX_SENDS {
            return Err(SelectRefusal {
                index: paths.len().min(MAX_SENDS),
                reason: SelectReason::TooMany,
            });
        }
        let mut files: Vec<Selected> = Vec::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            let refuse = |reason| SelectRefusal { index, reason };
            let before = match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(refuse(SelectReason::Missing));
                }
                Err(_) => return Err(refuse(SelectReason::Unreadable)),
            };
            let kind = before.file_type();
            if kind.is_symlink() {
                return Err(refuse(SelectReason::Symlink));
            }
            if kind.is_dir() {
                return Err(refuse(SelectReason::Directory));
            }
            if !kind.is_file() {
                return Err(refuse(SelectReason::SpecialFile));
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| portable(name))
                .ok_or(refuse(SelectReason::NotPortable))?;
            if files.iter().any(|selected| selected.name == name) {
                return Err(refuse(SelectReason::DuplicateName));
            }
            let file = File::from(
                rustix::fs::open(
                    path,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| refuse(SelectReason::Unreadable))?,
            );
            let after = file
                .metadata()
                .map_err(|_| refuse(SelectReason::Unreadable))?;
            if !after.file_type().is_file()
                || (after.dev(), after.ino()) != (before.dev(), before.ino())
            {
                return Err(refuse(SelectReason::Changed));
            }
            files.push(Selected {
                file,
                name: name.to_owned(),
                bytes: after.len(),
            });
        }
        Ok(Self { files })
    }
    pub fn len(&self) -> usize {
        self.files.len()
    }
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    /// Local sizes at selection time, in selection order (content-free).
    pub fn sizes(&self) -> impl ExactSizeIterator<Item = u64> + '_ {
        self.files.iter().map(|selected| selected.bytes)
    }
    /// A fresh request for ONE connection attempt: duplicated descriptors of
    /// the same selected files (the sender rewinds before hashing). Only an
    /// attempt that is granted control ever starts a source; an attempt that
    /// asked for control ends reconnection, so a selection is never resent.
    pub fn request(&self) -> Result<SendRequest, SelectRefusal> {
        let mut selection = batch::Selection::new();
        for (index, selected) in self.files.iter().enumerate() {
            let refuse = |reason| SelectRefusal { index, reason };
            let file = selected
                .file
                .try_clone()
                .map_err(|_| refuse(SelectReason::Unreadable))?;
            selection.push(file, &selected.name).map_err(|error| {
                refuse(match error {
                    sender::Error::Name => SelectReason::NotPortable,
                    _ => SelectReason::TooMany,
                })
            })?;
        }
        Ok(SendRequest {
            selection,
            policy: sender::Policy::default(),
            lifetime: SEND_LIFETIME,
        })
    }
}

/// One attempt's explicit selection for the viewer's drop lane.
pub struct SendRequest {
    pub(crate) selection: batch::Selection,
    pub(crate) policy: sender::Policy,
    pub(crate) lifetime: Duration,
}
impl fmt::Debug for SendRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSendRequest")
            .field("files", &self.selection.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendPhase {
    /// Configured; nothing happens before the original control grant.
    WaitingForControl,
    /// The drop expectation is set; the host's one-use offer is pending.
    Negotiating,
    /// The lane is up and the selection was handed to the batch sender.
    Sending,
    /// The batch report is complete (sources joined), whatever its outcome.
    Finished,
    /// No lane will carry this selection (see the absence).
    Ended,
}
/// Why no drop lane carries files on this session (either side). Content-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absence {
    /// The peer did not offer files (host without `--files`, or a controller
    /// that asked for none), so the file boundaries were not selected.
    NotNegotiated,
    /// Both clipboard and files were selected; this slice never sets up both.
    WithClipboard,
    /// Configured after service started, twice, or on a closed session.
    Unavailable,
    /// The drop expectation or the batch start was refused locally.
    Refused(sender::Error),
    /// The one-use exchange ended without a lane.
    SetupFailed(sender::Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendStatus {
    pub phase: SendPhase,
    pub absence: Option<Absence>,
    /// The batch's ordered, content-free receipts, retained after closure.
    pub report: Option<Report>,
}
/// Content-free UI handle for ONE attempt's selection. A clone never grants
/// authority, retries a file or starts another selection.
#[derive(Clone)]
pub struct SendControl(Arc<Mutex<SendStatus>>);
impl fmt::Debug for SendControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FileSendControl")
            .field(&self.status())
            .finish()
    }
}
impl SendControl {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(SendStatus {
            phase: SendPhase::WaitingForControl,
            absence: None,
            report: None,
        })))
    }
    fn lock(&self) -> MutexGuard<'_, SendStatus> {
        // Plain status; no caller code runs under this lock.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn status(&self) -> SendStatus {
        *self.lock()
    }
    pub(crate) fn update(&self, change: impl FnOnce(&mut SendStatus)) {
        change(&mut self.lock());
    }
    pub(crate) fn end(&self, absence: Absence) {
        self.update(|status| {
            status.phase = SendPhase::Ended;
            status.absence.get_or_insert(absence);
        });
    }
}

#[cfg(test)]
mod tests;
