//! Wire codec for `0x0075 SyncJobState` (`PROTOCOL.md` §6).
//!
//! Content-free state and ATP progress envelope for locally configured folder
//! synchronization jobs on the Files lane. Per `PROTOCOL.md` §6:
//! - Only the host may `Advertise` a locally configured job; controller
//!   acceptance can never create arbitrary host jobs or paths.
//! - Only explicit `KeepBoth` conflict preservation is valid in v0 (silent
//!   overwrite is unrepresentable on the wire).
//! - Observers are always refused (`WrongRole`); non-Files lanes are refused
//!   (`WrongChannel`); paths, filenames, and contents never appear here.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    files::{Context, Limits, Reason, Role},
    record::{Reader, Writer},
};

pub const CAPABILITY: &str = "file-folder-sync";
pub const VERSION: u16 = 1;
/// Fixed wire size including the 24-byte `FRD0` header:
/// 24 + 16 (`session`) + 16 (`lease`) + 16 (`handle`) + 8 (`job_id`) +
/// 1 (`sync_direction`) + 1 (`conflict_policy`) + 1 (`phase`) + 2 (`reason`) +
/// 4 (`entries_total`) + 4 (`entries_completed`) + 4 (`conflicts_kept`) +
/// 8 (`bytes_total`) + 8 (`bytes_completed`) + 8 (`manifest_revision`) = 121 bytes.
pub const RECORD_BYTES: usize = HEADER_BYTES + 16 + 16 + 16 + 8 + 1 + 1 + 1 + 2 + 4 + 4 + 4 + 8 + 8 + 8;

/// Direction of a locally configured folder-synchronization job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SyncDirection {
    HostToController = 1,
    ControllerToHost = 2,
    Bidirectional = 3,
}

impl SyncDirection {
    fn parse(v: u8) -> Result<Self, WireError> {
        match v {
            1 => Ok(Self::HostToController),
            2 => Ok(Self::ControllerToHost),
            3 => Ok(Self::Bidirectional),
            _ => Err(WireError::InvalidValue),
        }
    }
}

/// Explicit non-destructive conflict policy (`PROTOCOL.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConflictPolicy {
    /// Concurrent edits produce deterministic conflict copies; no silent overwrite.
    KeepBoth = 1,
}

impl ConflictPolicy {
    fn parse(v: u8) -> Result<Self, WireError> {
        match v {
            1 => Ok(Self::KeepBoth),
            _ => Err(WireError::InvalidValue),
        }
    }
}

/// Lifecycle phase of a synchronization job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SyncPhase {
    Advertised = 1,
    Accepted = 2,
    Scanning = 3,
    Transferring = 4,
    Settled = 5,
    ConflictRecorded = 6,
    Cancelled = 7,
    Failed = 8,
}

impl SyncPhase {
    fn parse(v: u8) -> Result<Self, WireError> {
        match v {
            1 => Ok(Self::Advertised),
            2 => Ok(Self::Accepted),
            3 => Ok(Self::Scanning),
            4 => Ok(Self::Transferring),
            5 => Ok(Self::Settled),
            6 => Ok(Self::ConflictRecorded),
            7 => Ok(Self::Cancelled),
            8 => Ok(Self::Failed),
            _ => Err(WireError::InvalidValue),
        }
    }

    const fn allows_sender(self, sender: Role) -> bool {
        match (self, sender) {
            // Only the host can advertise locally configured jobs; only the
            // controller can accept them. Observers are never permitted.
            (Self::Advertised, Role::Host) | (Self::Accepted, Role::Controller) => true,
            (
                Self::Scanning
                | Self::Transferring
                | Self::Settled
                | Self::ConflictRecorded
                | Self::Cancelled
                | Self::Failed,
                Role::Host | Role::Controller,
            ) => true,
            _ => false,
        }
    }
}

/// Content-free state report for one locally configured folder-sync job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncJobState {
    /// Nonzero locally configured job identifier.
    pub job_id: u64,
    pub sync_direction: SyncDirection,
    pub conflict_policy: ConflictPolicy,
    pub phase: SyncPhase,
    pub reason: Reason,
    pub entries_total: u32,
    pub entries_completed: u32,
    pub conflicts_kept: u32,
    pub bytes_total: u64,
    pub bytes_completed: u64,
    pub manifest_revision: u64,
}

impl SyncJobState {
    pub fn validate(&self, context: Context) -> Result<(), WireError> {
        context.validate()?;
        if !self.phase.allows_sender(context.sender) {
            return Err(WireError::WrongRole);
        }
        if self.job_id == 0
            || self.entries_completed > self.entries_total
            || self.conflicts_kept > self.entries_total
            || self.bytes_completed > self.bytes_total
        {
            return Err(WireError::InvalidValue);
        }
        let phase_ok = match self.phase {
            SyncPhase::Advertised | SyncPhase::Accepted => {
                self.reason == Reason::None
                    && self.entries_completed == 0
                    && self.bytes_completed == 0
                    && self.conflicts_kept == 0
            }
            SyncPhase::Scanning | SyncPhase::Transferring => {
                self.reason == Reason::None && self.manifest_revision != 0
            }
            SyncPhase::Settled => {
                self.reason == Reason::None
                    && self.manifest_revision != 0
                    && self.entries_completed == self.entries_total
                    && self.bytes_completed == self.bytes_total
            }
            SyncPhase::ConflictRecorded => {
                self.manifest_revision != 0 && self.conflicts_kept > 0
            }
            SyncPhase::Cancelled | SyncPhase::Failed => self.reason != Reason::None,
        };
        if !phase_ok {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}

fn read_u128(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}

pub fn encode(
    state: &SyncJobState,
    context: Context,
    limits: Limits,
    out: &mut [u8],
) -> Result<usize, WireError> {
    state.validate(context)?;
    let mut w = Writer::record_bounded(
        out,
        limits.record_bytes(),
        context.channel,
        Kind::SyncJobState,
        RECORD_BYTES - HEADER_BYTES,
    )?;
    w.put(&context.session.as_raw().to_be_bytes())?;
    w.put(&context.lease.as_raw().to_be_bytes())?;
    w.put(&context.handle.to_be_bytes())?;
    w.u64(state.job_id)?;
    w.u8(state.sync_direction as u8)?;
    w.u8(state.conflict_policy as u8)?;
    w.u8(state.phase as u8)?;
    w.u16(state.reason as u16)?;
    w.u32(state.entries_total)?;
    w.u32(state.entries_completed)?;
    w.u32(state.conflicts_kept)?;
    w.u64(state.bytes_total)?;
    w.u64(state.bytes_completed)?;
    w.u64(state.manifest_revision)?;
    w.finish()
}

pub fn decode(bytes: &[u8], context: Context, limits: Limits) -> Result<SyncJobState, WireError> {
    context.validate()?;
    let record = Record::decode_bounded(bytes, limits.record_bytes(), context.channel, None)?;
    let mut r = record.reader(Kind::SyncJobState)?;
    if read_u128(&mut r)? != context.session.as_raw()
        || read_u128(&mut r)? != context.lease.as_raw()
        || read_u128(&mut r)? != context.handle
    {
        return Err(WireError::InvalidBinding);
    }
    let state = SyncJobState {
        job_id: r.u64()?,
        sync_direction: SyncDirection::parse(r.u8()?)?,
        conflict_policy: ConflictPolicy::parse(r.u8()?)?,
        phase: SyncPhase::parse(r.u8()?)?,
        reason: Reason::parse(r.u16()?)?,
        entries_total: r.u32()?,
        entries_completed: r.u32()?,
        conflicts_kept: r.u32()?,
        bytes_total: r.u64()?,
        bytes_completed: r.u64()?,
        manifest_revision: r.u64()?,
    };
    r.finish()?;
    state.validate(context)?;
    Ok(state)
}
