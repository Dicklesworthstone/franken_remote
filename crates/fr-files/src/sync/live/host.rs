//! Source-version delivery on one original, explicitly accepted download lane.
use super::{
    ACCEPT_LIFETIME_US, CONTROL_LIFETIME_US, HostConfig, JobStatus, LiveError, MAX_SESSION_BYTES,
    MAX_SESSION_TRANSFERS,
    scan::{PreparedRound, Scan, ScanScope, Version},
    state,
};
use crate::{
    sender::{Outcome, Sender, batch},
    session::{Authority, Permission},
};
use asupersync::{cx::Cx, time::TimerDriverHandle};
use fr_transport::quic::{self, ConnectionBinding, Disposition, QuicRecords};
use fr_wire::{
    files::Reason,
    sync_job_state::{self, SyncDirection, SyncJobState, SyncPhase},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy)]
enum Step {
    Advertise,
    Accept,
    StartScan,
    Scanning,
    StartTransfer,
    Transfer,
    EndRound,
    Settlement,
    Waiting,
    Stopped,
}
struct PendingState {
    state: SyncJobState,
    deadline: u64,
    sent: bool,
    next: Step,
}

/// One host-configured source and its bounded, confirmed-version ledger.
/// A new connection cannot resume this owner, consent or an uncertain result.
pub struct HostJob {
    config: HostConfig,
    cx: Cx,
    clock: TimerDriverHandle,
    authority: Authority,
    permission: Permission,
    connection: ConnectionBinding,
    stopped: Arc<AtomicBool>,
    step: Step,
    pending: Option<PendingState>,
    scan: Option<Scan>,
    prepared: Option<PreparedRound>,
    versions: Vec<Version>,
    ledger: BTreeMap<String, String>,
    status: JobStatus,
    last: u64,
    acceptance_until: u64,
    round_until: u64,
    next_scan: u64,
    attempted_files: u32,
    attempted_bytes: u64,
    reported: usize,
    recorded_ledger: bool,
    cleanup: Option<Result<(), LiveError>>,
}
impl std::fmt::Debug for HostJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostFolderJob")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
impl HostJob {
    pub fn new(
        config: HostConfig,
        cx: Cx,
        authority: Authority,
        permission: Permission,
        q: &QuicRecords,
    ) -> Result<Self, LiveError> {
        cx.checkpoint().map_err(|_| LiveError::Cancelled)?;
        let clock = cx.timer_driver().ok_or(LiveError::Clock)?;
        let now = clock.now().as_nanos() / 1000;
        let acceptance_until = now
            .checked_add(ACCEPT_LIFETIME_US)
            .ok_or(LiveError::Clock)?;
        let status = JobStatus::new(config.job_id);
        Ok(Self {
            config,
            cx,
            clock,
            authority,
            permission,
            connection: q.binding(),
            stopped: Arc::new(AtomicBool::new(false)),
            step: Step::Advertise,
            pending: None,
            scan: None,
            prepared: None,
            versions: Vec::new(),
            ledger: BTreeMap::new(),
            status,
            last: now,
            acceptance_until,
            round_until: 0,
            next_scan: now,
            attempted_files: 0,
            attempted_bytes: 0,
            reported: 0,
            recorded_ledger: false,
            cleanup: None,
        })
    }
    pub const fn status(&self) -> JobStatus {
        self.status
    }

    /// Fence scanning and sending before cleanup. This never revokes input.
    pub fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.permission.revoke();
        self.prepared = None;
        self.pending = None;
        self.step = Step::Stopped;
        if self.status.failure.is_none() {
            self.status.fail(LiveError::Cancelled);
        }
    }

    pub fn service(
        &mut self,
        sender: &mut Sender<'_>,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), LiveError> {
        if !q.is_bound_to(&self.connection) {
            return Err(LiveError::WrongConnection);
        }
        let result = self.turn(sender, q, &mut authorize);
        if let Err(error) = result {
            self.status.fail(error);
            self.stop();
            // The ordinary sender retains every actual receipt, including one
            // collected in the turn that discovers expiry or cancellation.
            let _ = sender.cancel(q);
            let _ = self.try_finish_cleanup(sender);
        }
        result
    }

    fn turn(
        &mut self,
        sender: &mut Sender<'_>,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), LiveError> {
        if !q.is_bound_to(&self.connection) {
            return Err(LiveError::WrongConnection);
        }
        if let Some(error) = self.status.failure {
            return Err(error);
        }
        if self.stopped.load(Ordering::Acquire)
            || self.cx.checkpoint().is_err()
            || !self.permission.is_approved()
            || !authorize()
        {
            return Err(LiveError::Cancelled);
        }
        if !sender.sync_lane().sync_selected() {
            return Err(LiveError::NotNegotiated);
        }
        if sender.sync_lane().outgoing().direction != fr_wire::files::Direction::ToController
            || sender.sync_lane().outgoing().sender != fr_wire::files::Role::Host
        {
            return Err(LiveError::WrongDirection);
        }
        let context = sender.sync_lane().outgoing();
        if self.authority.direction() != fr_wire::files::Direction::ToHost
            || self.authority.binding().session != context.session
            || self.authority.binding().lease != context.lease
        {
            return Err(LiveError::WrongDirection);
        }
        let now = self.clock.now().as_nanos() / 1000;
        if now < self.last {
            return Err(LiveError::Clock);
        }
        self.last = now;
        self.authority
            .sample(now)
            .and_then(|at| self.authority.deadline(at))
            .map_err(|_| LiveError::Cancelled)?;
        sender.sync_lane().check(q).map_err(LiveError::Transport)?;
        if matches!(self.step, Step::Advertise | Step::Accept) && now >= self.acceptance_until {
            return Err(LiveError::Expired);
        }
        if matches!(
            self.step,
            Step::StartScan
                | Step::Scanning
                | Step::StartTransfer
                | Step::Transfer
                | Step::EndRound
                | Step::Settlement
        ) && now >= self.round_until
        {
            // Let the original sender reconcile an already queued terminal
            // proof before expiry stops it. Never start a successor or scan.
            if matches!(self.step, Step::Transfer) {
                let _ = sender.service(q, &mut *authorize);
                self.observe_report(sender)?;
            }
            return Err(LiveError::Expired);
        }
        if self.pending.is_some() {
            self.send_pending(sender, q, authorize, now)?;
            return Ok(());
        }
        self.advance(sender, q, authorize, now)?;
        if self.pending.is_some() {
            self.send_pending(sender, q, authorize, now)?;
        }
        Ok(())
    }

    /// Advance only after the original scope checks and any retained state
    /// record have completed. Newly queued state is sent by the enclosing turn.
    fn advance(
        &mut self,
        sender: &mut Sender<'_>,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
        now: u64,
    ) -> Result<(), LiveError> {
        match self.step {
            Step::Advertise => {
                self.queue(
                    state(self.config.job_id, SyncPhase::Advertised, 0),
                    Step::Accept,
                    now,
                )?;
            }
            Step::Accept => {
                if let Some(accepted) = self.receive_state(sender, q, authorize)? {
                    self.check_peer(accepted)?;
                    if accepted != state(self.config.job_id, SyncPhase::Accepted, 0) {
                        return Err(LiveError::WrongPhase);
                    }
                    self.start_round(now)?;
                }
            }
            Step::StartScan => {
                self.scan = Some(Scan::start(
                    &self.config,
                    self.status.state.manifest_revision,
                    self.ledger.clone(),
                    ScanScope {
                        cx: self.cx.clone(),
                        authority: self.authority.clone(),
                        permission: self.permission.clone(),
                        stopped: self.stopped.clone(),
                        until: self.round_until,
                    },
                )?);
                self.step = Step::Scanning;
            }
            Step::Scanning => self.poll_scan(now)?,
            Step::StartTransfer => {
                let prepared = self.prepared.take().ok_or(LiveError::WrongPhase)?;
                self.attempted_files +=
                    u32::try_from(prepared.versions.len()).map_err(|_| LiveError::Resource)?;
                self.attempted_bytes += prepared.bytes;
                self.versions = prepared.versions;
                self.reported = 0;
                self.recorded_ledger = false;
                sender
                    .begin_batch_before(q, prepared.selection, self.round_until)
                    .map_err(LiveError::Send)?;
                self.step = Step::Transfer;
            }
            Step::Transfer => self.service_transfer(sender, q, authorize)?,
            Step::EndRound => {
                if !sender.sync_ready(q).map_err(LiveError::Send)? {
                    return Ok(());
                }
                let end = self.status.state;
                if end.entries_completed != end.entries_total
                    || end.bytes_completed != end.bytes_total
                {
                    return Err(LiveError::PublicationUnknown);
                }
                self.queue(end, Step::Settlement, now)?;
            }
            Step::Settlement => {
                if let Some(settled) = self.receive_state(sender, q, authorize)? {
                    self.settle(settled, now)?;
                }
            }
            Step::Waiting => {
                // No unsolicited job state, proof or file request can grant a
                // new round. Check inbound even while the scan timer is idle.
                if self.receive_state(sender, q, authorize)?.is_some() {
                    return Err(LiveError::WrongPhase);
                }
                if now >= self.next_scan {
                    self.start_round(now)?;
                }
            }
            Step::Stopped => return Err(self.status.failure.unwrap_or(LiveError::Cancelled)),
        }
        Ok(())
    }

    fn settle(&mut self, settled: SyncJobState, now: u64) -> Result<(), LiveError> {
        self.check_peer(settled)?;
        let expected = self.status.state;
        if !matches!(
            settled.phase,
            SyncPhase::Settled | SyncPhase::ConflictRecorded
        ) || settled.reason != Reason::None
            || settled.manifest_revision != expected.manifest_revision
            || settled.entries_total != expected.entries_total
            || settled.entries_completed != expected.entries_total
            || settled.bytes_total != expected.bytes_total
            || settled.bytes_completed != expected.bytes_total
        {
            return Err(LiveError::WrongPhase);
        }
        self.status.state = settled;
        self.status.rounds_settled = self
            .status
            .rounds_settled
            .checked_add(1)
            .ok_or(LiveError::Resource)?;
        self.versions.clear();
        self.next_scan = now
            .checked_add(self.config.interval_us)
            .ok_or(LiveError::Clock)?;
        self.step = Step::Waiting;
        Ok(())
    }

    fn poll_scan(&mut self, now: u64) -> Result<(), LiveError> {
        let scanned = self.scan.as_mut().ok_or(LiveError::WrongPhase)?.poll();
        if scanned.is_err() {
            self.scan = None;
        }
        let Some(prepared) = scanned? else {
            return Ok(());
        };
        self.scan = None;
        let count = u32::try_from(prepared.versions.len()).map_err(|_| LiveError::Resource)?;
        if self
            .attempted_files
            .checked_add(count)
            .is_none_or(|n| n > MAX_SESSION_TRANSFERS)
            || self
                .attempted_bytes
                .checked_add(prepared.bytes)
                .is_none_or(|n| n > MAX_SESSION_BYTES)
        {
            return Err(LiveError::Resource);
        }
        let mut announcement = state(
            self.config.job_id,
            SyncPhase::Transferring,
            self.status.state.manifest_revision,
        );
        announcement.entries_total = count;
        announcement.bytes_total = prepared.bytes;
        let next = if count == 0 {
            Step::Settlement
        } else {
            Step::StartTransfer
        };
        self.prepared = (count != 0).then_some(prepared);
        self.queue(announcement, next, now)
    }

    fn service_transfer(
        &mut self,
        sender: &mut Sender<'_>,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), LiveError> {
        let turn = sender.service(q, &mut *authorize).map_err(LiveError::Send);
        self.observe_report(sender)?;
        turn?;
        if let Some(batch::Report {
            stop: Some(batch::Stop::Cleanup(error)),
            ..
        }) = sender.batch_report()
        {
            return Err(LiveError::Send(error));
        }
        if let Some(report) = sender.batch_report()
            && report.complete
        {
            if let Some(stop) = report.stop {
                return Err(LiveError::Transfer(stop));
            }
            if report.receipts().len() != self.versions.len() {
                return Err(LiveError::WrongPhase);
            }
            let _ = sender.take_batch_report();
            self.step = Step::EndRound;
        }
        Ok(())
    }

    fn start_round(&mut self, now: u64) -> Result<(), LiveError> {
        let revision = self
            .status
            .state
            .manifest_revision
            .checked_add(1)
            .ok_or(LiveError::Resource)?;
        self.round_until = now
            .checked_add(self.config.round_lifetime_us)
            .ok_or(LiveError::Clock)?;
        self.step = Step::StartScan;
        self.queue(
            state(self.config.job_id, SyncPhase::Scanning, revision),
            Step::StartScan,
            now,
        )
    }
    fn queue(&mut self, state: SyncJobState, next: Step, now: u64) -> Result<(), LiveError> {
        if self.pending.is_some() {
            return Err(LiveError::WrongPhase);
        }
        self.pending = Some(PendingState {
            state,
            next,
            sent: false,
            deadline: now
                .checked_add(CONTROL_LIFETIME_US)
                .ok_or(LiveError::Clock)?,
        });
        self.status.state = state;
        Ok(())
    }
    fn send_pending(
        &mut self,
        sender: &mut Sender<'_>,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
        now: u64,
    ) -> Result<(), LiveError> {
        let pending = self.pending.as_mut().ok_or(LiveError::WrongPhase)?;
        if now >= pending.deadline {
            return Err(LiveError::Expired);
        }
        if !pending.sent {
            if !sender.sync_ready(q).map_err(LiveError::Send)? {
                return Ok(());
            }
            match sender.sync_lane().send_sync(
                q,
                &self.cx,
                &pending.state,
                pending.deadline,
                authorize,
            ) {
                Ok(()) => pending.sent = true,
                Err(quic::Error::Backpressure) => return Ok(()),
                Err(error) => return Err(LiveError::Transport(error)),
            }
        }
        // The job record is still fully transport-counted. Do NOT run the
        // sender's ATP flight reconciliation until this same record drains.
        if sender
            .sync_lane()
            .send_drained(q)
            .map_err(LiveError::Transport)?
        {
            self.step = pending.next;
            self.pending = None;
        }
        Ok(())
    }
    fn receive_state(
        &self,
        sender: &Sender<'_>,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<Option<SyncJobState>, LiveError> {
        let lane = sender.sync_lane();
        let mut received = None;
        let mut failure = None;
        lane.dispatch(q, &self.cx, authorize, |bytes| {
            match sync_job_state::decode(bytes, lane.incoming(), lane.limits()) {
                Ok(state) => received = Some(state),
                Err(error) => failure = Some(LiveError::Wire(error)),
            }
            Ok(Disposition::Consumed)
        })
        .map_err(LiveError::Transport)?;
        match failure {
            Some(error) => Err(error),
            None => Ok(received),
        }
    }
    fn check_peer(&self, value: SyncJobState) -> Result<(), LiveError> {
        if value.job_id != self.config.job_id {
            return Err(LiveError::WrongJob);
        }
        if value.sync_direction != SyncDirection::HostToController {
            return Err(LiveError::WrongDirection);
        }
        if matches!(value.phase, SyncPhase::Cancelled | SyncPhase::Failed) {
            return Err(LiveError::Peer(value.reason));
        }
        Ok(())
    }
    fn observe_report(&mut self, sender: &Sender<'_>) -> Result<(), LiveError> {
        let Some(report) = sender.batch_report() else {
            return Ok(());
        };
        if report.total != self.versions.len() {
            return Err(LiveError::WrongPhase);
        }
        for (index, receipt) in report.receipts().enumerate().skip(self.reported) {
            if let Outcome::ControllerPublished { bytes, .. } = receipt.outcome {
                if bytes != self.versions[index].bytes {
                    return Err(LiveError::WrongPhase);
                }
                self.status.confirmed_publications = self
                    .status
                    .confirmed_publications
                    .checked_add(1)
                    .ok_or(LiveError::Resource)?;
                self.status.state.entries_completed += 1;
                self.status.state.bytes_completed = self
                    .status
                    .state
                    .bytes_completed
                    .checked_add(bytes)
                    .ok_or(LiveError::Resource)?;
            }
        }
        self.reported = report.receipts().len();
        if report.complete && !self.recorded_ledger {
            // complete is set ONLY after the original source's successful join.
            // Failed/lost publications never enter this ledger, and a confirmed
            // prefix survives a later failed entry without being replayed.
            for (version, receipt) in self.versions.iter().zip(report.receipts()) {
                if matches!(receipt.outcome, Outcome::ControllerPublished { .. }) {
                    self.ledger
                        .insert(version.name.clone(), version.hash.clone());
                }
            }
            self.recorded_ledger = true;
        }
        Ok(())
    }
    /// Nonblocking source-scan AND ordinary transfer cleanup. Later collection
    /// can strengthen publication evidence, never erase the original failure.
    pub fn try_finish_cleanup(&mut self, sender: &mut Sender<'_>) -> Option<Result<(), LiveError>> {
        if let Some(result) = self.cleanup {
            return Some(result);
        }
        if !self.stopped.load(Ordering::Acquire) {
            return None;
        }
        if let Some(scan) = &mut self.scan {
            match scan.poll() {
                Ok(None) => return None,
                Ok(Some(_)) | Err(LiveError::Source(_)) => self.scan = None,
                Err(error) => {
                    self.scan = None;
                    self.cleanup = Some(Err(error));
                    return self.cleanup;
                }
            }
        }
        let result = sender.try_finish_cleanup()?.map_err(LiveError::Send);
        let observed = self.observe_report(sender);
        let result = result.and(observed);
        self.status.cleanup_complete = result.is_ok();
        self.cleanup = Some(result);
        self.cleanup
    }
}
impl Drop for HostJob {
    fn drop(&mut self) {
        self.stop();
    }
}
