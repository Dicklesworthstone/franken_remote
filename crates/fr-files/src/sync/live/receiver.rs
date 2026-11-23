//! Explicit acceptance and real publication accounting on one original receiver.
use super::{ACCEPT_LIFETIME_US, CONTROL_LIFETIME_US, JobStatus, LiveError, MAX_ENTRIES, state};
use crate::{
    quic::{Receiver, State},
    receive::{ConflictPolicy, DropDirectory, Publication},
    wire::ResultReceipt,
    worker::{self, Completion},
};
use fr_transport::quic::{ConnectionBinding, Disposition, QuicRecords};
use fr_wire::{
    files::Reason,
    sync_job_state::{SyncDirection, SyncJobState, SyncPhase},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Advertisement,
    Accepting,
    Scan,
    Manifest,
    Files,
    Acknowledging,
    Stopped,
}
struct Pending {
    state: SyncJobState,
    deadline: u64,
    admitted: bool,
}

/// One explicitly accepted local job and pinned `KeepBoth` destination. Clones of
/// `directory()` belong only to this receiver lineage; no pathname is reopened.
/// The ordinary receiver still owns all staging, ATP integrity and permission.
pub struct ControllerJob {
    directory: DropDirectory,
    status: JobStatus,
    phase: Phase,
    pending: Option<Pending>,
    connection: Option<ConnectionBinding>,
    last_now: Option<u64>,
    advertised_until: Option<u64>,
    revision: u64,
    entries: u32,
    bytes: u64,
    completed: u32,
    published_bytes: u64,
    conflict_start: u64,
    last_published: u64,
    active: Option<(u64, u64)>,
}
impl ControllerJob {
    pub fn new(job_id: u64, directory: DropDirectory) -> Result<Self, LiveError> {
        if job_id == 0 {
            return Err(LiveError::InvalidConfig);
        }
        if directory.conflict_policy() != ConflictPolicy::KeepBoth {
            return Err(LiveError::KeepBothRequired);
        }
        Ok(Self {
            directory: directory.with_conflict_tracking(),
            status: JobStatus::new(job_id),
            phase: Phase::Advertisement,
            pending: None,
            connection: None,
            last_now: None,
            advertised_until: None,
            revision: 0,
            entries: 0,
            bytes: 0,
            completed: 0,
            published_bytes: 0,
            conflict_start: 0,
            last_published: 0,
            active: None,
        })
    }

    /// Install this exact tracked clone in the receiver BEFORE spawning its disk
    /// owner. Unrelated receive jobs cannot contribute conflict evidence.
    pub fn directory(&self) -> DropDirectory {
        self.directory.clone()
    }
    pub const fn status(&self) -> JobStatus {
        self.status
    }
    pub fn stop(&mut self) {
        if self.status.failure.is_none() {
            self.status.fail(LiveError::Cancelled);
        }
        self.phase = Phase::Stopped;
        self.pending = None;
    }
    /// The supervising receiver retains and reports the same joined result.
    pub fn cleaned(&mut self, result: Result<(), worker::Error>) {
        self.status.cleanup_complete = result.is_ok();
        if result.is_err() {
            self.status.fail(LiveError::Cleanup);
        }
    }

    /// Retain a final publication collected by this same fenced disk owner.
    /// This can increase actual effect counts after cancellation, but never
    /// acknowledges a round, sends state or resumes the job.
    pub fn collect_after_close(&mut self, receiver: &Receiver) -> Result<(), LiveError> {
        if !receiver.sync_tracks(&self.directory) {
            return Err(LiveError::InvalidConfig);
        }
        if self.phase != Phase::Stopped || receiver.state() == State::Active {
            return Err(LiveError::WrongPhase);
        }
        if let Some(receipt) = receiver.last_result()
            && receipt.id > self.last_published
            && let Ok(Completion::Published(published)) = receipt.outcome
            && let Err(error) = self.published(receipt.id, published)
        {
            self.status.fail(error);
            return Err(error);
        }
        Ok(())
    }

    pub fn service(
        &mut self,
        receiver: &mut Receiver,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), LiveError> {
        let result = self.turn(receiver, q, &mut authorize);
        if let Err(error) = result {
            // A foreign object supplies no authority to retire this original
            // lane. Every genuine job failure fences its own disk owner first.
            if !matches!(
                error,
                LiveError::WrongConnection
                    | LiveError::Receive(crate::quic::Error::WrongConnection)
            ) {
                self.status.fail(error);
                self.phase = Phase::Stopped;
                self.pending = None;
                receiver.stop();
                if let Err(retirement) = receiver.retire(q) {
                    return Err(LiveError::Receive(retirement));
                }
            }
        }
        result
    }

    fn turn(
        &mut self,
        receiver: &mut Receiver,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), LiveError> {
        if self
            .connection
            .as_ref()
            .is_some_and(|original| !q.is_bound_to(original))
        {
            return Err(LiveError::WrongConnection);
        }
        let now = receiver.sync_clock(q).map_err(LiveError::Receive)?;
        if !receiver.sync_tracks(&self.directory) {
            return Err(LiveError::InvalidConfig);
        }
        if self.connection.is_none() {
            self.connection = Some(q.binding());
        }
        if self.phase == Phase::Stopped {
            return Err(self.status.failure.unwrap_or(LiveError::Cancelled));
        }
        if self.last_now.is_some_and(|last| now < last) {
            return Err(LiveError::Clock);
        }
        self.last_now = Some(now);
        let advertised_until = if let Some(until) = self.advertised_until {
            until
        } else {
            let until = now
                .checked_add(ACCEPT_LIFETIME_US)
                .ok_or(LiveError::Clock)?;
            self.advertised_until = Some(until);
            until
        };
        if self.phase == Phase::Advertisement && now >= advertised_until {
            return Err(LiveError::Expired);
        }
        self.observe(receiver.last_result())?;
        self.finish_pending(receiver, q, now)?;
        let idle = receiver.sync_idle(q).map_err(LiveError::Receive)?;
        let allow_files = self.phase == Phase::Files && self.completed < self.entries;
        let mut incoming = None;
        let receiver_state = receiver
            .service_sync(q, &mut *authorize, allow_files, |record| {
                if !idle && !matches!(record.phase, SyncPhase::Cancelled | SyncPhase::Failed) {
                    return Ok(Disposition::Blocked);
                }
                incoming = Some(record);
                Ok(Disposition::Consumed)
            })
            .map_err(LiveError::Receive)?;
        self.observe(receiver.last_result())?;
        if let Some(record) = incoming {
            self.received(record, now)?;
        }
        if receiver_state != State::Active {
            return Err(LiveError::Cancelled);
        }
        if let Some(pending) = &mut self.pending {
            if now >= pending.deadline {
                return Err(LiveError::Expired);
            }
            if !pending.admitted {
                pending.admitted = receiver
                    .send_sync(q, &pending.state, pending.deadline, authorize)
                    .map_err(LiveError::Receive)?;
            }
        }
        Ok(())
    }

    fn finish_pending(
        &mut self,
        receiver: &Receiver,
        q: &QuicRecords,
        now: u64,
    ) -> Result<(), LiveError> {
        let Some(pending) = &self.pending else {
            return Ok(());
        };
        if now >= pending.deadline {
            return Err(LiveError::Expired);
        }
        if !pending.admitted || !receiver.sync_idle(q).map_err(LiveError::Receive)? {
            return Ok(());
        }
        self.pending = None;
        if self.phase == Phase::Acknowledging {
            self.status.rounds_settled = self
                .status
                .rounds_settled
                .checked_add(1)
                .ok_or(LiveError::Resource)?;
        } else if self.phase != Phase::Accepting {
            return Err(LiveError::WrongPhase);
        }
        self.phase = Phase::Scan;
        Ok(())
    }

    fn received(&mut self, record: SyncJobState, now: u64) -> Result<(), LiveError> {
        if record.job_id != self.status.state.job_id {
            return Err(LiveError::WrongJob);
        }
        if record.sync_direction != SyncDirection::HostToController {
            return Err(LiveError::WrongDirection);
        }
        if record.conflict_policy != fr_wire::sync_job_state::ConflictPolicy::KeepBoth {
            return Err(LiveError::KeepBothRequired);
        }
        if matches!(record.phase, SyncPhase::Cancelled | SyncPhase::Failed) {
            return Err(LiveError::Peer(record.reason));
        }
        if self.pending.is_some() || record.reason != Reason::None || record.conflicts_kept != 0 {
            return Err(LiveError::WrongPhase);
        }
        match self.phase {
            Phase::Advertisement => {
                if record != state(record.job_id, SyncPhase::Advertised, 0) {
                    return Err(LiveError::WrongPhase);
                }
                let acceptance = state(record.job_id, SyncPhase::Accepted, 0);
                self.queue(acceptance, now)?;
                self.phase = Phase::Accepting;
            }
            Phase::Scan => {
                let revision = self.revision.checked_add(1).ok_or(LiveError::Resource)?;
                if record != state(record.job_id, SyncPhase::Scanning, revision) {
                    return Err(LiveError::WrongPhase);
                }
                self.revision = revision;
                self.entries = 0;
                self.bytes = 0;
                self.completed = 0;
                self.published_bytes = 0;
                self.conflict_start = self.directory.conflicts_kept();
                self.active = None;
                self.status.state = record;
                self.phase = Phase::Manifest;
            }
            Phase::Manifest => {
                if record.phase != SyncPhase::Transferring
                    || record.manifest_revision != self.revision
                    || record.entries_total > MAX_ENTRIES
                    || record.entries_completed != 0
                    || record.bytes_completed != 0
                    || (record.entries_total == 0 && record.bytes_total != 0)
                {
                    return Err(LiveError::WrongPhase);
                }
                self.entries = record.entries_total;
                self.bytes = record.bytes_total;
                self.status.state = record;
                self.phase = Phase::Files;
                if self.entries == 0 {
                    self.acknowledge(now)?;
                }
            }
            Phase::Files => {
                if record.phase != SyncPhase::Transferring
                    || record.manifest_revision != self.revision
                    || record.entries_total != self.entries
                    || record.bytes_total != self.bytes
                    || record.entries_completed != self.entries
                    || record.bytes_completed != self.bytes
                {
                    return Err(LiveError::WrongPhase);
                }
                self.acknowledge(now)?;
            }
            _ => return Err(LiveError::WrongPhase),
        }
        Ok(())
    }

    fn observe(&mut self, receipt: Option<ResultReceipt>) -> Result<(), LiveError> {
        let Some(receipt) = receipt else {
            return Ok(());
        };
        if receipt.id <= self.last_published {
            return Ok(());
        }
        if self.phase != Phase::Files {
            return Err(LiveError::WrongPhase);
        }
        match receipt.outcome {
            Ok(Completion::Begun(progress)) => {
                if self.completed >= self.entries
                    || progress.id != receipt.id
                    || self
                        .published_bytes
                        .checked_add(progress.total_bytes)
                        .is_none_or(|bytes| bytes > self.bytes)
                    || self
                        .active
                        .is_some_and(|active| active != (receipt.id, progress.total_bytes))
                {
                    return Err(LiveError::Resource);
                }
                self.active = Some((receipt.id, progress.total_bytes));
            }
            Ok(Completion::Written(progress)) => {
                if self.active != Some((receipt.id, progress.total_bytes))
                    || progress.id != receipt.id
                {
                    return Err(LiveError::WrongPhase);
                }
            }
            Ok(Completion::Published(published)) => {
                self.published(receipt.id, published)?;
            }
            Ok(Completion::Cancelled) => return Err(LiveError::Cancelled),
            Err(worker::Error::UnknownEffect | worker::Error::Panicked) => {
                return Err(LiveError::PublicationUnknown);
            }
            Err(_) => return Err(LiveError::Worker),
        }
        Ok(())
    }

    fn published(&mut self, id: u64, published: crate::session::Receipt) -> Result<(), LiveError> {
        if self.active != Some((id, published.bytes)) || published.id != id {
            return Err(LiveError::WrongPhase);
        }
        self.last_published = id;
        self.active = None;
        self.completed = self.completed.checked_add(1).ok_or(LiveError::Resource)?;
        self.published_bytes = self
            .published_bytes
            .checked_add(published.bytes)
            .ok_or(LiveError::Resource)?;
        self.status.confirmed_publications = self
            .status
            .confirmed_publications
            .checked_add(1)
            .ok_or(LiveError::Resource)?;
        self.status.state.entries_completed = self.completed;
        self.status.state.bytes_completed = self.published_bytes;
        if published.publication != Publication::Durable {
            return Err(LiveError::DurabilityUnknown);
        }
        Ok(())
    }

    fn acknowledge(&mut self, now: u64) -> Result<(), LiveError> {
        if self.active.is_some()
            || self.completed != self.entries
            || self.published_bytes != self.bytes
        {
            return Err(LiveError::WrongPhase);
        }
        let conflicts = self
            .directory
            .conflicts_kept()
            .checked_sub(self.conflict_start)
            .and_then(|count| u32::try_from(count).ok())
            .ok_or(LiveError::Resource)?;
        if conflicts > self.completed {
            return Err(LiveError::Resource);
        }
        let reply = SyncJobState {
            phase: if conflicts == 0 {
                SyncPhase::Settled
            } else {
                SyncPhase::ConflictRecorded
            },
            entries_completed: self.completed,
            bytes_completed: self.published_bytes,
            conflicts_kept: conflicts,
            ..self.status.state
        };
        self.queue(reply, now)?;
        self.phase = Phase::Acknowledging;
        Ok(())
    }

    fn queue(&mut self, record: SyncJobState, now: u64) -> Result<(), LiveError> {
        if self.pending.is_some() {
            return Err(LiveError::WrongPhase);
        }
        let deadline = now
            .checked_add(CONTROL_LIFETIME_US)
            .ok_or(LiveError::Clock)?;
        self.status.state = record;
        self.pending = Some(Pending {
            state: record,
            deadline,
            admitted: false,
        });
        Ok(())
    }
}
