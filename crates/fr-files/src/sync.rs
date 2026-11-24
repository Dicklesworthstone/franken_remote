//! Bounded, non-destructive ATP folder synchronization (`PROTOCOL.md` §6 `0x0075 SyncJobState`).
//!
//! Design invariants (plan §15.6, `AGENTS.md` §4 & §6):
//! - Both endpoints pin their own local directory by descriptor (`DropDirectory`,
//!   `ConflictPolicy::KeepBoth`). Wire records never carry filesystem paths,
//!   filenames, or file contents.
//! - Symlinks, special files, and subdirectories (unsupported in v0) are refused
//!   with typed errors rather than followed or silently skipped.
//! - Concurrent edits and occupied destinations are NEVER silently overwritten:
//!   publication uses `DropDirectory::begin_manifest` + `PendingFile::verify` +
//!   `PendingFile::publish_named_checked` under `ConflictPolicy::KeepBoth`.
//! - Live authority is checked before scanning, before every file, before every
//!   chunk write, and immediately before each OS atomic rename. Revoking the
//!   lease mid-transfer cancels staging without leaving partial published files
//!   and emits a valid `0x0075 SyncJobState` (`Cancelled` / `Reason::Permission`).

#[cfg(unix)]
use crate::receive::ConflictPolicy;
use crate::receive::{DropDirectory, Error as ReceiveError, MAX_CHUNK_BYTES, Publication};
#[cfg(unix)]
mod cache;
#[cfg(unix)]
mod projection;
#[cfg(unix)]
mod source;
#[cfg(unix)]
use fr_core::limits::ProtocolLimits;
use fr_wire::WireError;
use fr_wire::files::{Context as WireContext, Reason as WireReason, Role as WireRole};
use fr_wire::sync_job_state::{
    ConflictPolicy as WireConflictPolicy, RECORD_BYTES, SyncDirection, SyncJobState, SyncPhase,
};
#[cfg(unix)]
use fr_wire::{files::Limits as WireLimits, sync_job_state};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::Path;

/// Explicitly accepted, one-way jobs on the original native download lane.
pub mod live;
pub use live::{ControllerJob, HostConfig, HostJob, JobStatus, LiveError};

#[cfg(unix)]
const STAGING_PREFIX: &str = ".fr-part-";

/// Resource limits enforced before scanning, staging, or transferring entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncLimits {
    pub max_entries: u32,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_chunk_bytes: usize,
}

impl Default for SyncLimits {
    fn default() -> Self {
        Self {
            max_entries: 256,
            max_file_bytes: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_chunk_bytes: MAX_CHUNK_BYTES,
        }
    }
}

impl SyncLimits {
    pub fn validate(self) -> Result<(), SyncError> {
        if self.max_entries == 0
            || self.max_file_bytes == 0
            || self.max_total_bytes < self.max_file_bytes
            || self.max_chunk_bytes == 0
            || self.max_chunk_bytes > MAX_CHUNK_BYTES
        {
            return Err(SyncError::InvalidConfig);
        }
        Ok(())
    }
}

/// Locally configured folder-synchronization job parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncJobConfig {
    /// Nonzero locally configured job identifier.
    pub job_id: u64,
    pub direction: SyncDirection,
    pub conflict_policy: WireConflictPolicy,
    pub limits: SyncLimits,
}

impl SyncJobConfig {
    pub fn validate(&self) -> Result<(), SyncError> {
        if self.job_id == 0 {
            return Err(SyncError::InvalidConfig);
        }
        self.limits.validate()
    }
}

/// Sanitized sync failures: never contain filenames, paths, hashes, or payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncError {
    InvalidConfig,
    InvalidRevision,
    InvalidRoot,
    UnsafeRoot,
    RootChanged,
    SourceChanged,
    UnsupportedPlatform,
    InvalidEntryName,
    SymlinkForbidden,
    SubdirectoryUnsupported,
    UnsupportedEntryKind,
    EntryLimitExceeded,
    FileSizeLimitExceeded,
    TotalBytesLimitExceeded,
    AuthorityRevoked,
    KeepBothRequired,
    Receive(ReceiveError),
    Wire(WireError),
    Io(io::ErrorKind),
}

impl SyncError {
    /// Map a sync failure to the wire `files::Reason` code for `0x0075 SyncJobState`.
    pub const fn wire_reason(self) -> WireReason {
        match self {
            Self::AuthorityRevoked => WireReason::Permission,
            Self::EntryLimitExceeded
            | Self::FileSizeLimitExceeded
            | Self::TotalBytesLimitExceeded
            | Self::Receive(ReceiveError::Quota) => WireReason::Resource,
            Self::SourceChanged | Self::Receive(ReceiveError::Integrity) => WireReason::Integrity,
            Self::Receive(ReceiveError::Conflict) => WireReason::Conflict,
            Self::InvalidConfig
            | Self::InvalidRevision
            | Self::InvalidEntryName
            | Self::SymlinkForbidden
            | Self::SubdirectoryUnsupported
            | Self::UnsupportedEntryKind
            | Self::InvalidRoot
            | Self::UnsafeRoot
            | Self::RootChanged
            | Self::UnsupportedPlatform
            | Self::KeepBothRequired
            | Self::Receive(
                ReceiveError::InvalidName | ReceiveError::InvalidRoot | ReceiveError::UnsafeRoot,
            ) => WireReason::Invalid,
            Self::Wire(_) | Self::Receive(_) | Self::Io(_) => WireReason::UnknownEffect,
        }
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "folder-sync: {self:?}")
    }
}

impl std::error::Error for SyncError {}

impl From<ReceiveError> for SyncError {
    fn from(value: ReceiveError) -> Self {
        Self::Receive(value)
    }
}

impl From<WireError> for SyncError {
    fn from(value: WireError) -> Self {
        Self::Wire(value)
    }
}

impl From<io::Error> for SyncError {
    fn from(value: io::Error) -> Self {
        Self::Io(value.kind())
    }
}

#[cfg(unix)]
impl From<crate::rx::io::Errno> for SyncError {
    fn from(value: crate::rx::io::Errno) -> Self {
        Self::Io(io::Error::from(value).kind())
    }
}

/// Content-addressed metadata for one regular file in a scanned folder.
/// `Debug` omits the filename and hashes (`AGENTS.md` §6).
#[derive(Clone, PartialEq, Eq)]
pub struct ManifestFile {
    name: String,
    size: u64,
    sha256_hex: String,
    merkle_root_hex: String,
    #[cfg(unix)]
    original: source::Identity,
}

impl ManifestFile {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn size(&self) -> u64 {
        self.size
    }

    pub fn sha256_hex(&self) -> &str {
        &self.sha256_hex
    }

    pub fn merkle_root_hex(&self) -> &str {
        &self.merkle_root_hex
    }
}

impl fmt::Debug for ManifestFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManifestFile")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// Deterministic, bounded snapshot of regular files in a locally pinned folder.
/// `Debug` omits filenames, paths, and digests.
#[derive(Clone, PartialEq, Eq)]
pub struct FolderManifest {
    revision: u64,
    entries: BTreeMap<String, ManifestFile>,
    total_bytes: u64,
    root_hash_hex: String,
    #[cfg(unix)]
    inspected_entries: u32,
}

impl FolderManifest {
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn entries(&self) -> &BTreeMap<String, ManifestFile> {
        &self.entries
    }

    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn root_hash_hex(&self) -> &str {
        &self.root_hash_hex
    }
}

impl fmt::Debug for FolderManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FolderManifest")
            .field("revision", &self.revision)
            .field("entry_count", &self.entries.len())
            .field("total_bytes", &self.total_bytes)
            .finish_non_exhaustive()
    }
}

/// Last settled content-hash baseline for deterministic 3-way reconciliation.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SyncBaseline {
    entries: BTreeMap<String, String>,
    /// Versions already published under keep-both names. Receipt custody is
    /// separate from equal-name hashes: the originals deliberately still differ.
    kept: Vec<KeptVersion>,
}

#[derive(Clone, PartialEq, Eq)]
struct KeptVersion {
    leg: TransferLeg,
    original_name: String,
    sha256_hex: String,
}

impl SyncBaseline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_manifest(manifest: &FolderManifest) -> Self {
        let entries = manifest
            .entries
            .iter()
            .map(|(name, file)| (name.clone(), file.sha256_hex.clone()))
            .collect();
        Self {
            entries,
            kept: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn hash_for(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(String::as_str)
    }

    pub fn record(&mut self, name: String, sha256_hex: String) {
        self.entries.insert(name, sha256_hex);
    }

    fn already_kept(&self, leg: TransferLeg, file: &ManifestFile) -> bool {
        self.kept.iter().any(|kept| {
            kept.leg == leg && kept.original_name == file.name && kept.sha256_hex == file.sha256_hex
        })
    }

    #[cfg(unix)]
    fn record_kept(&mut self, transfer: &PlannedTransfer, published_name: &str) {
        // A conflict copy is a known local-only name. It must not bounce back
        // to its source as a new file on the next bidirectional round.
        self.record(published_name.to_owned(), transfer.file.sha256_hex.clone());
        self.kept
            .retain(|kept| kept.leg != transfer.leg || kept.original_name != transfer.file.name);
        self.kept.push(KeptVersion {
            leg: transfer.leg,
            original_name: transfer.file.name.clone(),
            sha256_hex: transfer.file.sha256_hex.clone(),
        });
    }
}

impl fmt::Debug for SyncBaseline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncBaseline")
            .field("entry_count", &self.entries.len())
            .field("kept_version_count", &self.kept.len())
            .finish_non_exhaustive()
    }
}

/// Endpoint direction of a single planned copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferLeg {
    HostToController,
    ControllerToHost,
}

/// One planned transfer step in deterministic filename order.
#[derive(Clone, PartialEq, Eq)]
pub struct PlannedTransfer {
    pub leg: TransferLeg,
    pub expects_conflict_copy: bool,
    pub file: ManifestFile,
}

impl fmt::Debug for PlannedTransfer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlannedTransfer")
            .field("leg", &self.leg)
            .field("expects_conflict_copy", &self.expects_conflict_copy)
            .field("size", &self.file.size)
            .finish_non_exhaustive()
    }
}

/// Deterministic reconciliation plan computed from `(baseline, host, controller, direction)`.
#[derive(Clone, PartialEq, Eq)]
pub struct SyncPlan {
    pub transfers: Vec<PlannedTransfer>,
    pub in_sync_count: u32,
    pub preserved_local_only_count: u32,
    pub skipped_by_direction_count: u32,
    pub planned_bytes: u64,
}

impl fmt::Debug for SyncPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncPlan")
            .field("transfer_count", &self.transfers.len())
            .field("in_sync_count", &self.in_sync_count)
            .field(
                "preserved_local_only_count",
                &self.preserved_local_only_count,
            )
            .field(
                "skipped_by_direction_count",
                &self.skipped_by_direction_count,
            )
            .field("planned_bytes", &self.planned_bytes)
            .finish_non_exhaustive()
    }
}

/// Result of publishing one file during a sync round.
#[derive(Clone, PartialEq, Eq)]
pub struct PublishedSyncEntry {
    pub leg: TransferLeg,
    original_name: String,
    published_name: String,
    pub size: u64,
    pub publication: Publication,
    pub conflict_copy: bool,
}

impl PublishedSyncEntry {
    pub fn original_name(&self) -> &str {
        &self.original_name
    }

    pub fn published_name(&self) -> &str {
        &self.published_name
    }
}

impl fmt::Debug for PublishedSyncEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublishedSyncEntry")
            .field("leg", &self.leg)
            .field("size", &self.size)
            .field("publication", &self.publication)
            .field("conflict_copy", &self.conflict_copy)
            .finish_non_exhaustive()
    }
}

/// Wire-validated `0x0075 SyncJobState` emission along with its exact 121-byte frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedWireState {
    pub sender: WireRole,
    pub state: SyncJobState,
    pub frame: [u8; RECORD_BYTES],
}

/// Outcome of one bounded folder-synchronization round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRoundOutcome {
    pub manifest_revision: u64,
    pub final_phase: SyncPhase,
    pub published: Vec<PublishedSyncEntry>,
    pub conflicts_kept: u32,
    pub bytes_transferred: u64,
    pub baseline: SyncBaseline,
    pub wire_states: Vec<EmittedWireState>,
}

/// Failure outcome paired with the validated terminal `0x0075 SyncJobState` records
/// emitted up through cancellation or failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRoundFailure {
    pub error: SyncError,
    /// Staging removal or original reservation accounting failed. The cause
    /// and committed publications remain available, but cleanup is unconfirmed.
    pub cleanup_failed: bool,
    /// Publications already committed before failure. They are not rolled back
    /// and must be retained when reporting cancellation or deciding to retry.
    pub published: Vec<PublishedSyncEntry>,
    /// Original baseline advanced only by confirmed publications, for a retry
    /// under newly checked authority without duplicating committed conflicts.
    pub baseline: SyncBaseline,
    pub wire_states: Vec<EmittedWireState>,
}

/// Compute a deterministic 3-way reconciliation plan.
///
/// Non-destructive guarantees:
/// - Never emits delete operations.
/// - Files absent on one side that match the last baseline hash are treated as
///   local-only removals and preserved without overwriting or resurrecting.
/// - Divergent files on both sides never silently overwrite: when direction
///   permits transfer onto an occupied destination, the transfer is marked
///   `expects_conflict_copy = true` and materialized via `DropDirectory`'s
///   atomic `ConflictPolicy::KeepBoth` rename.
pub fn plan_sync(
    baseline: &SyncBaseline,
    host: &FolderManifest,
    controller: &FolderManifest,
    direction: SyncDirection,
) -> Result<SyncPlan, SyncError> {
    let names: BTreeSet<&str> = host
        .entries
        .keys()
        .chain(controller.entries.keys())
        .map(String::as_str)
        .collect();

    let allow_h2c = direction != SyncDirection::ControllerToHost;
    let allow_c2h = direction != SyncDirection::HostToController;

    let mut transfers = Vec::new();
    let mut in_sync_count = 0u32;
    let mut preserved_local_only_count = 0u32;
    let mut skipped_by_direction_count = 0u32;
    let mut planned_bytes = 0u64;

    let mut push_transfer = |leg: TransferLeg,
                             expects_conflict_copy: bool,
                             file: &ManifestFile|
     -> Result<(), SyncError> {
        if baseline.already_kept(leg, file) {
            // Preserve the receipt even if the user subsequently edits or
            // deletes the received copy. An unchanged original is not a new
            // transfer request and must not recreate that copy indefinitely,
            // including when the destination's original name is later removed.
            return Ok(());
        }
        planned_bytes = planned_bytes
            .checked_add(file.size)
            .ok_or(SyncError::TotalBytesLimitExceeded)?;
        transfers.push(PlannedTransfer {
            leg,
            expects_conflict_copy,
            file: file.clone(),
        });
        Ok(())
    };

    for name in names {
        match (host.entries.get(name), controller.entries.get(name)) {
            (Some(h), Some(c)) => {
                if h.sha256_hex == c.sha256_hex {
                    in_sync_count = in_sync_count
                        .checked_add(1)
                        .ok_or(SyncError::EntryLimitExceeded)?;
                    continue;
                }
                let base_hash = baseline.hash_for(name);
                match direction {
                    SyncDirection::HostToController => {
                        push_transfer(TransferLeg::HostToController, true, h)?;
                    }
                    SyncDirection::ControllerToHost => {
                        push_transfer(TransferLeg::ControllerToHost, true, c)?;
                    }
                    SyncDirection::Bidirectional => {
                        if base_hash == Some(h.sha256_hex.as_str()) {
                            // Only controller modified since baseline; deliver to host without overwrite.
                            push_transfer(TransferLeg::ControllerToHost, true, c)?;
                        } else if base_hash == Some(c.sha256_hex.as_str()) {
                            // Only host modified since baseline; deliver to controller without overwrite.
                            push_transfer(TransferLeg::HostToController, true, h)?;
                        } else {
                            // True concurrent edit or divergent initial state: keep both on both sides.
                            push_transfer(TransferLeg::HostToController, true, h)?;
                            push_transfer(TransferLeg::ControllerToHost, true, c)?;
                        }
                    }
                }
            }
            (Some(h), None) => {
                if baseline.hash_for(name) == Some(h.sha256_hex.as_str()) {
                    preserved_local_only_count = preserved_local_only_count
                        .checked_add(1)
                        .ok_or(SyncError::EntryLimitExceeded)?;
                } else if allow_h2c {
                    push_transfer(TransferLeg::HostToController, false, h)?;
                } else {
                    skipped_by_direction_count = skipped_by_direction_count
                        .checked_add(1)
                        .ok_or(SyncError::EntryLimitExceeded)?;
                }
            }
            (None, Some(c)) => {
                if baseline.hash_for(name) == Some(c.sha256_hex.as_str()) {
                    preserved_local_only_count = preserved_local_only_count
                        .checked_add(1)
                        .ok_or(SyncError::EntryLimitExceeded)?;
                } else if allow_c2h {
                    push_transfer(TransferLeg::ControllerToHost, false, c)?;
                } else {
                    skipped_by_direction_count = skipped_by_direction_count
                        .checked_add(1)
                        .ok_or(SyncError::EntryLimitExceeded)?;
                }
            }
            (None, None) => {}
        }
    }

    Ok(SyncPlan {
        transfers,
        in_sync_count,
        preserved_local_only_count,
        skipped_by_direction_count,
        planned_bytes,
    })
}

/// Open and scan a local directory through one retained descriptor.
///
/// This convenience form has no session authority. Session callers should use
/// [`scan_directory_checked`] or [`execute_sync_round`]. Unix roots must be
/// absolute, owned by this user and not group/other writable. Other platforms
/// refuse until descriptor-relative directory enumeration is implemented.
pub fn scan_directory(
    root: &Path,
    revision: u64,
    limits: SyncLimits,
) -> Result<FolderManifest, SyncError> {
    scan_directory_checked(root, revision, limits, || Ok(()))
}

/// Bound directory enumeration and file hashing with the original authority.
/// `max_entries` includes inspected private staging entries; each bounded read
/// is bracketed by `check_authority`. No pathname is reopened during the scan.
pub fn scan_directory_checked<F>(
    root: &Path,
    revision: u64,
    limits: SyncLimits,
    mut check_authority: F,
) -> Result<FolderManifest, SyncError>
where
    F: FnMut() -> Result<(), SyncError>,
{
    limits.validate()?;
    if revision == 0 {
        return Err(SyncError::InvalidRevision);
    }
    if !root.is_absolute() {
        return Err(SyncError::InvalidRoot);
    }
    check_authority()?;
    #[cfg(unix)]
    {
        use crate::rx::fd::AsFd;
        let root = source::open_root(root)?;
        source::scan(root.as_fd(), revision, limits, &mut check_authority)
    }
    #[cfg(not(unix))]
    Err(SyncError::UnsupportedPlatform)
}

/// Execute one authority-fenced folder-synchronization round between locally
/// pinned host and controller roots, emitting real wire-validated `0x0075 SyncJobState`
/// frames at every phase transition.
///
/// Both `DropDirectory` instances are forced to `ConflictPolicy::KeepBoth`
/// (silent overwrite is structurally impossible). `check_authority` runs before
/// scanning, before each transfer, before every chunk write, and inside
/// `PendingFile::publish_named_checked` immediately before each OS rename.
#[cfg(unix)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn execute_sync_round<F>(
    config: &SyncJobConfig,
    host_wire_context: WireContext,
    controller_wire_context: WireContext,
    host_root: &Path,
    host_drop: &DropDirectory,
    controller_root: &Path,
    controller_drop: &DropDirectory,
    baseline: &SyncBaseline,
    manifest_revision: u64,
    mut check_authority: F,
) -> Result<SyncRoundOutcome, SyncRoundFailure>
where
    F: FnMut() -> Result<(), SyncError>,
{
    let mut wire_states = Vec::new();
    let mut published = Vec::new();
    let mut next_baseline = baseline.clone();
    let host_drop = host_drop
        .clone()
        .with_conflict_policy(ConflictPolicy::KeepBoth)
        .with_observed_cleanup();
    let controller_drop = controller_drop
        .clone()
        .with_conflict_policy(ConflictPolicy::KeepBoth)
        .with_observed_cleanup();
    let cleanup_failed = || host_drop.cleanup_failed() || controller_drop.cleanup_failed();
    if let Err(err) = validate_round_inputs(
        config,
        host_wire_context,
        controller_wire_context,
        manifest_revision,
    ) {
        return Err(SyncRoundFailure {
            error: err,
            cleanup_failed: cleanup_failed(),
            published,
            baseline: next_baseline,
            wire_states,
        });
    }

    let fail_with = |error: SyncError,
                     entries_total: u32,
                     entries_completed: u32,
                     conflicts_kept: u32,
                     bytes_total: u64,
                     bytes_completed: u64,
                     revision: u64,
                     published: Vec<PublishedSyncEntry>,
                     baseline: SyncBaseline,
                     wire_states: &mut Vec<EmittedWireState>|
     -> SyncRoundFailure {
        let cleanup_failed = cleanup_failed();
        let phase = if error == SyncError::AuthorityRevoked && !cleanup_failed {
            SyncPhase::Cancelled
        } else {
            SyncPhase::Failed
        };
        let state = SyncJobState {
            job_id: config.job_id,
            sync_direction: config.direction,
            conflict_policy: config.conflict_policy,
            phase,
            reason: if cleanup_failed {
                WireReason::UnknownEffect
            } else {
                error.wire_reason()
            },
            entries_total,
            entries_completed,
            conflicts_kept,
            bytes_total,
            bytes_completed,
            manifest_revision: revision,
        };
        let _ = emit_wire_state(host_wire_context, state, wire_states);
        SyncRoundFailure {
            error,
            cleanup_failed,
            published,
            baseline,
            wire_states: std::mem::take(wire_states),
        }
    };

    // An existing supervising owner must never lose an earlier cleanup fault
    // by creating another local sync round over the same selected directory.
    if cleanup_failed() {
        return Err(fail_with(
            SyncError::Receive(ReceiveError::Retired),
            0,
            0,
            0,
            0,
            0,
            0,
            published,
            next_baseline,
            &mut wire_states,
        ));
    }

    // 1. Authority check prior to advertisement / scanning.
    if let Err(err) = check_authority() {
        return Err(fail_with(
            err,
            0,
            0,
            0,
            0,
            0,
            0,
            published,
            next_baseline,
            &mut wire_states,
        ));
    }

    // 2. Host advertises the locally configured job; controller accepts it.
    let advertised = SyncJobState {
        job_id: config.job_id,
        sync_direction: config.direction,
        conflict_policy: config.conflict_policy,
        phase: SyncPhase::Advertised,
        reason: WireReason::None,
        entries_total: 0,
        entries_completed: 0,
        conflicts_kept: 0,
        bytes_total: 0,
        bytes_completed: 0,
        manifest_revision: 0,
    };
    if let Err(err) = emit_wire_state(host_wire_context, advertised, &mut wire_states) {
        return Err(SyncRoundFailure {
            error: err,
            cleanup_failed: cleanup_failed(),
            published,
            baseline: next_baseline,
            wire_states,
        });
    }

    let accepted = SyncJobState {
        phase: SyncPhase::Accepted,
        ..advertised
    };
    if let Err(err) = emit_wire_state(controller_wire_context, accepted, &mut wire_states) {
        return Err(SyncRoundFailure {
            error: err,
            cleanup_failed: cleanup_failed(),
            published,
            baseline: next_baseline,
            wire_states,
        });
    }

    // 3. Emit Scanning phase and scan both locally pinned roots.
    let scanning = SyncJobState {
        phase: SyncPhase::Scanning,
        manifest_revision,
        ..advertised
    };
    if let Err(err) = emit_wire_state(host_wire_context, scanning, &mut wire_states) {
        return Err(SyncRoundFailure {
            error: err,
            cleanup_failed: cleanup_failed(),
            published,
            baseline: next_baseline,
            wire_states,
        });
    }

    if let Err(error) = source::check_root(host_root, &host_drop)
        .and_then(|()| source::check_root(controller_root, &controller_drop))
    {
        return Err(fail_with(
            error,
            0,
            0,
            0,
            0,
            0,
            manifest_revision,
            published,
            next_baseline,
            &mut wire_states,
        ));
    }
    let host_manifest = match source::scan(
        host_drop.root_fd(),
        manifest_revision,
        config.limits,
        &mut check_authority,
    ) {
        Ok(m) => m,
        Err(err) => {
            return Err(fail_with(
                err,
                0,
                0,
                0,
                0,
                0,
                manifest_revision,
                published,
                next_baseline,
                &mut wire_states,
            ));
        }
    };
    let controller_manifest = match source::scan(
        controller_drop.root_fd(),
        manifest_revision,
        config.limits,
        &mut check_authority,
    ) {
        Ok(m) => m,
        Err(err) => {
            return Err(fail_with(
                err,
                0,
                0,
                0,
                0,
                0,
                manifest_revision,
                published,
                next_baseline,
                &mut wire_states,
            ));
        }
    };

    let plan = match plan_sync(
        baseline,
        &host_manifest,
        &controller_manifest,
        config.direction,
    ) {
        Ok(p) => p,
        Err(err) => {
            return Err(fail_with(
                err,
                0,
                0,
                0,
                0,
                0,
                manifest_revision,
                published,
                next_baseline,
                &mut wire_states,
            ));
        }
    };

    let entries_total = match u32::try_from(plan.transfers.len()) {
        Ok(n) if n <= config.limits.max_entries.saturating_mul(2) => n,
        _ => {
            return Err(fail_with(
                SyncError::EntryLimitExceeded,
                0,
                0,
                0,
                0,
                0,
                manifest_revision,
                published,
                next_baseline,
                &mut wire_states,
            ));
        }
    };
    let bytes_total = plan.planned_bytes;
    if bytes_total > config.limits.max_total_bytes.saturating_mul(2) {
        return Err(fail_with(
            SyncError::TotalBytesLimitExceeded,
            entries_total,
            0,
            0,
            0,
            0,
            manifest_revision,
            published,
            next_baseline,
            &mut wire_states,
        ));
    }

    // Publication must not make an unchanged next round exceed its own scan
    // limits. Check both destination projections before committing either leg.
    if let Err(error) =
        projection::validate(&plan, &host_manifest, &controller_manifest, config.limits)
    {
        return Err(fail_with(
            error,
            entries_total,
            0,
            0,
            bytes_total,
            0,
            manifest_revision,
            published,
            next_baseline,
            &mut wire_states,
        ));
    }

    // Keep the baseline aligned with the scanned, bounded names before any copy.
    // Retain local-only deletion history while a name exists on either side,
    // but do not grow the baseline forever as unrelated names disappear.
    next_baseline.entries.retain(|name, _| {
        host_manifest.entries.contains_key(name) || controller_manifest.entries.contains_key(name)
    });
    next_baseline.kept.retain(|kept| {
        let source = match kept.leg {
            TransferLeg::HostToController => &host_manifest,
            TransferLeg::ControllerToHost => &controller_manifest,
        };
        source
            .entries
            .get(&kept.original_name)
            .is_some_and(|file| file.sha256_hex == kept.sha256_hex)
    });
    for (name, h) in &host_manifest.entries {
        if controller_manifest
            .entries
            .get(name)
            .is_some_and(|c| c.sha256_hex == h.sha256_hex)
        {
            next_baseline.record(name.clone(), h.sha256_hex.clone());
        }
    }

    // 4. Transfer each planned entry with chunk-level and publication-time authority gates.
    published.reserve_exact(plan.transfers.len());
    let mut entries_completed = 0u32;
    let mut conflicts_kept = 0u32;
    let mut bytes_completed = 0u64;

    for transfer in &plan.transfers {
        if let Err(err) = check_authority() {
            return Err(fail_with(
                err,
                entries_total,
                entries_completed,
                conflicts_kept,
                bytes_total,
                bytes_completed,
                manifest_revision,
                published,
                next_baseline,
                &mut wire_states,
            ));
        }

        let (src_root, dst_drop, sender_ctx) = match transfer.leg {
            TransferLeg::HostToController => {
                (host_drop.root_fd(), &controller_drop, host_wire_context)
            }
            TransferLeg::ControllerToHost => (
                controller_drop.root_fd(),
                &host_drop,
                controller_wire_context,
            ),
        };

        let pub_file = match source::transfer(
            src_root,
            dst_drop,
            &transfer.file,
            config.limits.max_chunk_bytes,
            &mut check_authority,
        ) {
            Ok(pf) => pf,
            Err(err) => {
                return Err(fail_with(
                    err,
                    entries_total,
                    entries_completed,
                    conflicts_kept,
                    bytes_total,
                    bytes_completed,
                    manifest_revision,
                    published,
                    next_baseline,
                    &mut wire_states,
                ));
            }
        };

        let conflict_copy = pub_file.was_renamed();
        // Advance custody at the commit, not only at a successful round's end.
        // A later cancellation or wire error must return this exact baseline.
        if conflict_copy {
            next_baseline.record_kept(transfer, pub_file.name());
        } else {
            next_baseline.record(transfer.file.name.clone(), transfer.file.sha256_hex.clone());
        }
        if conflict_copy {
            conflicts_kept = conflicts_kept.saturating_add(1);
        }
        entries_completed = entries_completed.saturating_add(1);
        bytes_completed = bytes_completed.saturating_add(transfer.file.size);

        published.push(PublishedSyncEntry {
            leg: transfer.leg,
            original_name: transfer.file.name.clone(),
            published_name: pub_file.name().to_owned(),
            size: transfer.file.size,
            publication: pub_file.publication(),
            conflict_copy,
        });

        // Publication is already committed even if its final reservation
        // accounting failed. Retain it, then report failed cleanup separately.
        if cleanup_failed() {
            return Err(fail_with(
                SyncError::Receive(ReceiveError::Retired),
                entries_total,
                entries_completed,
                conflicts_kept,
                bytes_total,
                bytes_completed,
                manifest_revision,
                published,
                next_baseline,
                &mut wire_states,
            ));
        }

        let progress = SyncJobState {
            job_id: config.job_id,
            sync_direction: config.direction,
            conflict_policy: config.conflict_policy,
            phase: SyncPhase::Transferring,
            reason: WireReason::None,
            entries_total,
            entries_completed,
            conflicts_kept,
            bytes_total,
            bytes_completed,
            manifest_revision,
        };
        if let Err(err) = emit_wire_state(sender_ctx, progress, &mut wire_states) {
            return Err(SyncRoundFailure {
                error: err,
                cleanup_failed: cleanup_failed(),
                published,
                baseline: next_baseline,
                wire_states,
            });
        }
    }

    // 6. Emit final terminal wire state: ConflictRecorded if any KeepBoth copy was created, else Settled.
    let final_phase = if conflicts_kept > 0 {
        SyncPhase::ConflictRecorded
    } else {
        SyncPhase::Settled
    };
    let final_state = SyncJobState {
        job_id: config.job_id,
        sync_direction: config.direction,
        conflict_policy: config.conflict_policy,
        phase: final_phase,
        reason: WireReason::None,
        entries_total,
        entries_completed,
        conflicts_kept,
        bytes_total,
        bytes_completed,
        manifest_revision,
    };
    if let Err(err) = emit_wire_state(host_wire_context, final_state, &mut wire_states) {
        return Err(SyncRoundFailure {
            error: err,
            cleanup_failed: cleanup_failed(),
            published,
            baseline: next_baseline,
            wire_states,
        });
    }

    Ok(SyncRoundOutcome {
        manifest_revision,
        final_phase,
        published,
        conflicts_kept,
        bytes_transferred: bytes_completed,
        baseline: next_baseline,
        wire_states,
    })
}

#[cfg(unix)]
fn validate_round_inputs(
    config: &SyncJobConfig,
    host_wire_context: WireContext,
    controller_wire_context: WireContext,
    manifest_revision: u64,
) -> Result<(), SyncError> {
    config.validate()?;
    if manifest_revision == 0 {
        return Err(SyncError::InvalidRevision);
    }
    if host_wire_context.sender != WireRole::Host
        || controller_wire_context.sender != WireRole::Controller
    {
        return Err(SyncError::Wire(WireError::WrongRole));
    }
    host_wire_context.validate()?;
    controller_wire_context.validate()?;
    Ok(())
}

#[cfg(unix)]
fn emit_wire_state(
    context: WireContext,
    state: SyncJobState,
    out: &mut Vec<EmittedWireState>,
) -> Result<(), SyncError> {
    let limits = WireLimits::new(&ProtocolLimits::ABSOLUTE, 4096)?;
    let mut frame = [0u8; RECORD_BYTES];
    let written = sync_job_state::encode(&state, context, limits, &mut frame)?;
    if written != RECORD_BYTES {
        return Err(SyncError::Wire(WireError::Truncated));
    }
    out.push(EmittedWireState {
        sender: context.sender,
        state,
        frame,
    });
    Ok(())
}

/// Folder synchronization needs a qualified descriptor-relative directory
/// scanner. Regular-file receive remains available on Windows separately.
#[cfg(not(unix))]
#[allow(clippy::too_many_arguments)]
pub fn execute_sync_round<F>(
    _config: &SyncJobConfig,
    _host_wire_context: WireContext,
    _controller_wire_context: WireContext,
    _host_root: &Path,
    _host_drop: &DropDirectory,
    _controller_root: &Path,
    _controller_drop: &DropDirectory,
    baseline: &SyncBaseline,
    _manifest_revision: u64,
    _check_authority: F,
) -> Result<SyncRoundOutcome, SyncRoundFailure>
where
    F: FnMut() -> Result<(), SyncError>,
{
    Err(SyncRoundFailure {
        error: SyncError::UnsupportedPlatform,
        cleanup_failed: false,
        published: Vec::new(),
        baseline: baseline.clone(),
        wire_states: Vec::new(),
    })
}
