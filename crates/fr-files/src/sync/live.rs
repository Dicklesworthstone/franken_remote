//! One-way, locally selected folder jobs on an existing controlled file lane.
//! Recursive jobs publish each top-level directory as one atomic ATP tree version.
//!
//! A settled round confirms delivery of its selected source versions. It does
//! not assert that the two directories are identical: independent client edits
//! and removals remain local, and acknowledged versions are never replayed to
//! undo them. The job, its consent and its version ledger never cross a session.
use super::{SyncError, SyncLimits};
use crate::{quic, receive::DropDirectory, sender};
use fr_wire::{
    WireError,
    files::Reason,
    sync_job_state::{ConflictPolicy, JobMode, SyncDirection, SyncJobState, SyncPhase},
};
use std::{fmt, path::Path, time::Duration};

mod host;
mod paging;
mod receiver;
mod scan;
pub use host::HostJob;
pub use receiver::ControllerJob;

/// A pinned local source for either positively negotiated one-way direction.
pub type SourceConfig = HostConfig;
/// Source scanning and ATP delivery; use `for_controller` for an upload job.
pub type SourceJob = HostJob;
/// Publication accounting; use `for_host` for a host-advertised upload job.
pub type ReceiverJob = ControllerJob;

/// A source scan is bounded independently from one network delivery round.
pub const MAX_SOURCE_ENTRIES: u32 = 256;
/// Every advertised round still fits the sender's single bounded batch.
pub const MAX_ENTRIES: u32 = 32;
const _: () = assert!(sender::batch::MAX_FILES == MAX_ENTRIES as usize);
/// Cumulative attempted versions for an explicitly selected folder job. This
/// permits a complete source plus updates without resetting any byte budget.
/// Independent native receiving limits may be lower; refusal remains terminal.
pub const MAX_SESSION_TRANSFERS: u32 = 512;
pub const MAX_SESSION_BYTES: u64 = 1024 * 1024 * 1024;
pub(super) const ROUND_LIFETIME_US: u64 = 300_000_000;
pub(super) const CONTROL_LIFETIME_US: u64 = 1_000_000;
pub(super) const ACCEPT_LIFETIME_US: u64 = 5_000_000;

/// Each endpoint charges the declared round once, before admitting any of its
/// files. Empty, settled rounds can continue after a budget is exhausted.
#[derive(Default)]
struct JobBudget {
    files: u32,
    bytes: u64,
}

impl JobBudget {
    fn admit(&mut self, files: u32, bytes: u64) -> Result<(), LiveError> {
        let limits = SyncLimits::default();
        if files > MAX_ENTRIES
            || bytes > limits.max_total_bytes
            || bytes > u64::from(files) * limits.max_file_bytes
            || files > MAX_SESSION_TRANSFERS - self.files
            || bytes > MAX_SESSION_BYTES - self.bytes
        {
            return Err(LiveError::Resource);
        }
        self.files += files;
        self.bytes += bytes;
        Ok(())
    }
}

/// Sanitized terminal job failures. None contains a local path or source hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveError {
    InvalidConfig,
    KeepBothRequired,
    NotNegotiated,
    WrongConnection,
    WrongJob,
    WrongDirection,
    WrongPhase,
    Clock,
    Expired,
    Cancelled,
    Resource,
    Source(SyncError),
    Send(sender::Error),
    Receive(quic::Error),
    Transport(fr_transport::quic::Error),
    Wire(WireError),
    Worker,
    Panicked,
    Cleanup,
    PublicationUnknown,
    DurabilityUnknown,
    Transfer(sender::batch::Stop),
    Peer(Reason),
}
impl LiveError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidConfig => "sync_invalid_config",
            Self::KeepBothRequired => "sync_keep_both_required",
            Self::NotNegotiated => "sync_not_negotiated",
            Self::WrongConnection => "sync_reconnect_refused",
            Self::WrongJob => "sync_job_mismatch",
            Self::WrongDirection => "sync_direction_unsupported",
            Self::WrongPhase => "sync_state_refused",
            Self::Clock => "sync_clock",
            Self::Expired => "sync_expired",
            Self::Cancelled => "sync_cancelled",
            Self::Resource => "sync_resource_limit",
            Self::Source(_) => "sync_source_refused",
            Self::Send(_) | Self::Receive(_) | Self::Transport(_) | Self::Wire(_) => {
                "sync_transfer_failed"
            }
            Self::Worker | Self::Panicked => "sync_worker_failed",
            Self::Cleanup => "sync_cleanup_unconfirmed",
            Self::PublicationUnknown => "sync_publication_unknown",
            Self::DurabilityUnknown => "sync_durability_unconfirmed",
            Self::Transfer(_) => "sync_transfer_stopped",
            Self::Peer(_) => "sync_peer_failed",
        }
    }
    pub const fn wire_reason(self) -> Reason {
        match self {
            Self::Cancelled => Reason::Permission,
            Self::Expired => Reason::Expired,
            Self::Resource => Reason::Resource,
            Self::Source(error) => error.wire_reason(),
            Self::Peer(reason) => reason,
            Self::InvalidConfig
            | Self::KeepBothRequired
            | Self::NotNegotiated
            | Self::WrongConnection
            | Self::WrongJob
            | Self::WrongDirection
            | Self::WrongPhase
            | Self::Wire(_) => Reason::Invalid,
            _ => Reason::UnknownEffect,
        }
    }
}
impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for LiveError {}

/// Bounded, content-free observations from the original job owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobStatus {
    pub state: SyncJobState,
    pub failure: Option<LiveError>,
    pub confirmed_publications: u32,
    /// Original-session keep-both publications. The host learns this count
    /// from settled rounds; the receiver learns it from its disk owner.
    pub confirmed_conflicts: u32,
    pub rounds_settled: u64,
    pub cleanup_complete: bool,
}
impl JobStatus {
    pub(super) fn new(job_id: u64, direction: SyncDirection) -> Self {
        Self {
            state: state(job_id, direction, SyncPhase::Advertised, 0),
            failure: None,
            confirmed_publications: 0,
            confirmed_conflicts: 0,
            rounds_settled: 0,
            cleanup_complete: false,
        }
    }
    pub(super) fn fail(&mut self, error: LiveError) {
        let error = *self.failure.get_or_insert(error);
        self.state.phase = if error == LiveError::Cancelled {
            SyncPhase::Cancelled
        } else {
            SyncPhase::Failed
        };
        self.state.reason = error.wire_reason();
    }
}

pub(super) fn state(
    job_id: u64,
    direction: SyncDirection,
    phase: SyncPhase,
    revision: u64,
) -> SyncJobState {
    SyncJobState {
        job_id,
        sync_direction: direction,
        conflict_policy: ConflictPolicy::KeepBoth,
        phase,
        reason: Reason::None,
        entries_total: 0,
        entries_completed: 0,
        conflicts_kept: 0,
        bytes_total: 0,
        bytes_completed: 0,
        manifest_revision: revision,
    }
}

/// One explicit local source, opened without reading any of its entries.
/// The root descriptor remains the source after pathname replacement. Scanning
/// starts only after the original host advertisement and matching explicit
/// controller acceptance have completed on the selected file lane.
pub struct HostConfig {
    pub(super) job_id: u64,
    pub(super) root: DropDirectory,
    pub(super) limits: SyncLimits,
    pub(super) recursive: bool,
    pub(super) interval_us: u64,
    pub(super) round_lifetime_us: u64,
}
impl fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FolderSyncSource")
            .field("job_id", &self.job_id)
            .field("limits", &self.limits)
            .field("recursive", &self.recursive)
            .finish_non_exhaustive()
    }
}
impl HostConfig {
    pub fn open(path: &Path, job_id: u64) -> Result<Self, LiveError> {
        if !cfg!(unix) {
            return Err(LiveError::Source(SyncError::UnsupportedPlatform));
        }
        if job_id == 0 || !path.is_absolute() {
            return Err(LiveError::InvalidConfig);
        }
        let limits = SyncLimits {
            max_entries: MAX_SOURCE_ENTRIES,
            ..SyncLimits::default()
        };
        let root = DropDirectory::open(
            path,
            crate::receive::Limits {
                max_file_bytes: limits.max_file_bytes,
                max_reserved_bytes: limits.max_total_bytes,
                max_transfers: 1,
            },
        )
        .map_err(|error| LiveError::Source(SyncError::Receive(error)))?;
        Ok(Self {
            job_id,
            root,
            limits,
            recursive: false,
            interval_us: 5_000_000,
            round_lifetime_us: ROUND_LIFETIME_US,
        })
    }
    /// Explicitly select atomic subtree versions as well as regular files.
    /// Both original endpoints must negotiate the recursive job version before
    /// scanning; this never upgrades an ordinary or flat file attachment.
    pub fn with_recursive(self) -> Self {
        self.with_mode(JobMode::Recursive)
    }

    /// Carry the locally chosen command scope to the original file attachment.
    #[must_use]
    pub fn with_mode(mut self, mode: JobMode) -> Self {
        self.recursive = mode == JobMode::Recursive;
        self
    }
    pub const fn mode(&self) -> JobMode {
        JobMode::from_recursive(self.recursive)
    }
    pub const fn is_recursive(&self) -> bool {
        self.recursive
    }
    pub const fn job_id(&self) -> u64 {
        self.job_id
    }
    /// Locally configured scan pacing; a new round never overlaps its prior
    /// scan, transfer, publication proof, source cleanup or state exchange.
    pub fn with_scan_interval(mut self, interval: Duration) -> Result<Self, LiveError> {
        let micros = u64::try_from(interval.as_micros()).map_err(|_| LiveError::InvalidConfig)?;
        if !(100_000..=300_000_000).contains(&micros) {
            return Err(LiveError::InvalidConfig);
        }
        self.interval_us = micros;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_reservations_are_bounded_and_rejections_spend_nothing() {
        let limits = SyncLimits::default();
        let mut budget = JobBudget::default();
        for (entries, bytes) in [
            (0, 1),
            (1, limits.max_file_bytes + 1),
            (MAX_ENTRIES, limits.max_total_bytes + 1),
            (MAX_ENTRIES + 1, 0),
            (1, u64::MAX),
        ] {
            assert_eq!(budget.admit(entries, bytes), Err(LiveError::Resource));
        }
        for _ in 0..4 {
            budget.admit(4, limits.max_total_bytes).unwrap();
        }
        assert_eq!(budget.admit(1, 1), Err(LiveError::Resource));
        for _ in 0..(MAX_SESSION_TRANSFERS - 16) / MAX_ENTRIES {
            budget.admit(MAX_ENTRIES, 0).unwrap();
        }
        budget
            .admit((MAX_SESSION_TRANSFERS - 16) % MAX_ENTRIES, 0)
            .unwrap();
        assert_eq!(budget.admit(1, 0), Err(LiveError::Resource));
        budget.admit(0, 0).unwrap();
    }
}
