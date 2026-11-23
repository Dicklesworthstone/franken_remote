//! One-way, locally selected folder jobs on an existing controlled download.
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
    sync_job_state::{ConflictPolicy, SyncDirection, SyncJobState, SyncPhase},
};
use std::{fmt, path::Path, time::Duration};

mod host;
mod receiver;
mod scan;
pub use host::HostJob;
pub use receiver::ControllerJob;

pub const MAX_ENTRIES: u32 = sender::batch::MAX_FILES as u32;
/// Standing work retains the native session's cumulative transfer budget.
/// A full initial scan must leave room for later source versions on this lane.
pub const MAX_SESSION_TRANSFERS: u32 = 64;
pub const MAX_SESSION_BYTES: u64 = 1024 * 1024 * 1024;
pub(super) const CONTROL_LIFETIME_US: u64 = 1_000_000;
pub(super) const ACCEPT_LIFETIME_US: u64 = 5_000_000;

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
    pub rounds_settled: u64,
    pub cleanup_complete: bool,
}
impl JobStatus {
    pub(super) fn new(job_id: u64) -> Self {
        Self {
            state: state(job_id, SyncPhase::Advertised, 0),
            failure: None,
            confirmed_publications: 0,
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

pub(super) fn state(job_id: u64, phase: SyncPhase, revision: u64) -> SyncJobState {
    SyncJobState {
        job_id,
        sync_direction: SyncDirection::HostToController,
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
/// starts only after the original controller explicitly accepts this job ID.
pub struct HostConfig {
    pub(super) job_id: u64,
    pub(super) root: DropDirectory,
    pub(super) limits: SyncLimits,
    pub(super) interval_us: u64,
    pub(super) round_lifetime_us: u64,
}
impl fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FolderSyncSource")
            .field("job_id", &self.job_id)
            .field("limits", &self.limits)
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
            max_entries: MAX_ENTRIES,
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
            interval_us: 5_000_000,
            round_lifetime_us: 300_000_000,
        })
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
