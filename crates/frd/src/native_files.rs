//! Explicit files and portable directory roots on the original control lease.
//!
//! Local source selection and local destination consent precede network startup.
//! Legacy upload uses file-channel-scope v1; host download requires the positive
//! file-download v1 capability. The immutable one-use attachment fixes direction.
//! Both paths use the existing ATP full-object profiles and bounded disk owners.
//! Private staging is published atomically without replacing an existing name,
//! only while the original accepted grant and its bounded view remain live.
//!
//! A batch is consumed once, in order, and stops at its first failure. Clipboard
//! and files have independent consent, tickets, workers and cancellation. Neither
//! grants authority to the other. There is no automatic replay, resumption,
//! browsing, synchronization or picker UI. Native copy drops can submit fresh
//! selections on the original upload intake. Status retains actual publication
//! separately from progress, refusal and cleanup, without names or contents.
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
    time::Duration,
};

pub use fr_files::receive::{ConflictPolicy, Publication};
pub use fr_files::sender::batch::Report;

/// At most this many selections in an initial `--send` or later native drop (the batch owner's
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
    /// A host-operator choice, never an option taken from the transfer manifest.
    /// The pinned directory and shared reservations survive changes on a clone;
    /// changing this preference grants no new input or file authority.
    #[must_use]
    pub fn with_conflict_policy(mut self, policy: ConflictPolicy) -> Self {
        self.directory = self.directory.with_conflict_policy(policy);
        self
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
    /// Legacy refusal code; portable directories are now selectable.
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

enum SelectedKind {
    File(u64),
    Directory,
}
struct Selected {
    file: File,
    name: String,
    kind: SelectedKind,
}
/// The user's explicit local selection: open regular-file/directory descriptors and
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
    /// symbolic links and special files refuse by type. Open directories with
    /// `O_DIRECTORY`, and every selection with `O_NOFOLLOW | O_NONBLOCK` (a
    /// racing FIFO cannot block us). Require the descriptor to retain its
    /// classified type and identity. No file content or directory is read here;
    /// tree traversal, structural bounds, link rejection and hashing run on the
    /// sender's bounded disk thread only after control is granted.
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
            if !kind.is_file() && !kind.is_dir() {
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
            let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            if kind.is_dir() {
                flags |= OFlags::DIRECTORY;
            }
            let file = File::from(
                rustix::fs::open(path, flags, Mode::empty())
                    .map_err(|_| refuse(SelectReason::Unreadable))?,
            );
            let after = file
                .metadata()
                .map_err(|_| refuse(SelectReason::Unreadable))?;
            if after.file_type() != kind
                || (after.dev(), after.ino()) != (before.dev(), before.ino())
            {
                return Err(refuse(SelectReason::Changed));
            }
            files.push(Selected {
                file,
                name: name.to_owned(),
                kind: if kind.is_dir() {
                    SelectedKind::Directory
                } else {
                    SelectedKind::File(after.len())
                },
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
    /// Local file sizes at selection time, in order (content-free). Directory
    /// content sizes remain unknown until the authorized worker walks the tree;
    /// a directory inode's metadata length is never reported as payload bytes.
    pub fn sizes(&self) -> impl ExactSizeIterator<Item = Option<u64>> + '_ {
        self.files.iter().map(|selected| match selected.kind {
            SelectedKind::File(bytes) => Some(bytes),
            SelectedKind::Directory => None,
        })
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
            let queued = match selected.kind {
                SelectedKind::File(_) => selection.push(file, &selected.name),
                SelectedKind::Directory => selection.push_directory(file, &selected.name),
            };
            queued.map_err(|error| {
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
    /// Legacy absence from builds that could not multiplex both attachments.
    /// Current negotiation does not return this variant.
    WithClipboard,
    /// Configured after service started, twice, or on a closed session.
    Unavailable,
    /// The drop expectation or the batch start was refused locally.
    Refused(sender::Error),
    /// The one-use exchange ended without a lane.
    SetupFailed(sender::Error),
    /// The attached local receiver ended; retained publication results still apply.
    ReceiveFailed(fr_files::quic::Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendStatus {
    pub phase: SendPhase,
    pub absence: Option<Absence>,
    /// The batch's ordered, content-free receipts, retained after closure.
    pub report: Option<Report>,
}
mod control;
pub use control::{SendControl, SendSnapshot};
mod intake;
pub use intake::SendIntake;
mod download;
pub use download::{
    HostSelection, MAX_RECEIVE_RESULTS, ReceiveControl, ReceivePhase, ReceiveRequest, ReceiveStatus,
};

#[cfg(test)]
mod tests;
