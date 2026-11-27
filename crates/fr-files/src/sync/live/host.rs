//! Source-version delivery on one original, explicitly accepted file lane.
use super::{
    ACCEPT_LIFETIME_US, HostConfig, JobBudget, JobStatus, LiveError,
    paging::Cursor,
    scan::{PreparedRound, Scan, ScanCache, ScanScope, Version},
    state, state_deadline,
};
use crate::{
    sender::{Outcome, Sender, SourceScope, batch},
    session::{Authority, Permission},
    sync::SyncError,
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
    Advertisement,
    BeginRound,
    StartScan,
    Scanning,
    StartTransfer,
    PreparingTransfer,
    DiscardTransfer,
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

const MAX_SCAN_ATTEMPTS: u8 = 3;
const SCAN_RETRY_DELAY_US: u64 = 100_000;

/// A failed, joined scan or unoffered first source may inspect a newer
/// version, but never acquire a new round deadline or refund spent budgets.
#[derive(Default)]
struct ScanRetry {
    attempts: u8,
    not_before: u64,
}
impl ScanRetry {
    fn begin(&mut self, now: u64, round_until: u64) -> Result<bool, LiveError> {
        if now >= round_until {
            return Err(LiveError::Expired);
        }
        if now < self.not_before {
            return Ok(false);
        }
        if self.attempts >= MAX_SCAN_ATTEMPTS {
            return Err(LiveError::Source(SyncError::SourceChanged));
        }
        self.attempts += 1;
        Ok(true)
    }

    fn defer(&mut self, error: LiveError, now: u64, round_until: u64) -> Result<(), LiveError> {
        if error != LiveError::Source(SyncError::SourceChanged)
            || self.attempts >= MAX_SCAN_ATTEMPTS
        {
            return Err(error);
        }
        let not_before = now
            .checked_add(SCAN_RETRY_DELAY_US)
            .ok_or(LiveError::Clock)?;
        if not_before >= round_until {
            return Err(LiveError::Expired);
        }
        self.not_before = not_before;
        Ok(())
    }
}

/// One locally selected source and its bounded, confirmed-version ledger.
/// The host advertises download jobs; an upload source accepts a host-advertised
/// destination. Neither path scans before that original handshake completes.
/// A new connection cannot resume this owner, consent or an uncertain result.
pub struct HostJob {
    config: HostConfig,
    direction: SyncDirection,
    cx: Cx,
    clock: TimerDriverHandle,
    authority: Authority,
    permission: Permission,
    connection: ConnectionBinding,
    stopped: Arc<AtomicBool>,
    step: Step,
    pending: Option<PendingState>,
    scan: Option<Scan>,
    scan_cache: ScanCache,
    scan_retry: ScanRetry,
    cursor: Cursor,
    prepared: Option<PreparedRound>,
    versions: Vec<Version>,
    ledger: BTreeMap<String, String>,
    status: JobStatus,
    last: u64,
    acceptance_until: u64,
    round_until: u64,
    next_scan: u64,
    budget: JobBudget,
    reported: usize,
    recorded_ledger: bool,
    cleanup: Option<Result<(), LiveError>>,
}
impl std::fmt::Debug for HostJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceFolderJob")
            .field("direction", &self.direction)
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
        Self::with_direction(
            config,
            cx,
            authority,
            permission,
            q,
            SyncDirection::HostToController,
        )
    }

    /// Explicitly accept one host-advertised job with this pinned local source.
    /// The accepted controller authority must belong to this original upload
    /// channel. A selected source alone never advertises or creates a host job.
    pub fn for_controller(
        config: HostConfig,
        cx: Cx,
        authority: Authority,
        permission: Permission,
        q: &QuicRecords,
    ) -> Result<Self, LiveError> {
        Self::with_direction(
            config,
            cx,
            authority,
            permission,
            q,
            SyncDirection::ControllerToHost,
        )
    }

    fn with_direction(
        config: HostConfig,
        cx: Cx,
        authority: Authority,
        permission: Permission,
        q: &QuicRecords,
        direction: SyncDirection,
    ) -> Result<Self, LiveError> {
        cx.checkpoint().map_err(|_| LiveError::Cancelled)?;
        let clock = cx.timer_driver().ok_or(LiveError::Clock)?;
        let now = clock.now().as_nanos() / 1000;
        let acceptance_until = now
            .checked_add(ACCEPT_LIFETIME_US)
            .ok_or(LiveError::Clock)?;
        let status = JobStatus::new(config.job_id, direction);
        Ok(Self {
            config,
            direction,
            cx,
            clock,
            authority,
            permission,
            connection: q.binding(),
            stopped: Arc::new(AtomicBool::new(false)),
            step: if direction == SyncDirection::HostToController {
                Step::Advertise
            } else {
                Step::Advertisement
            },
            pending: None,
            scan: None,
            scan_cache: ScanCache::default(),
            scan_retry: ScanRetry::default(),
            cursor: Cursor::default(),
            prepared: None,
            versions: Vec::new(),
            ledger: BTreeMap::new(),
            status,
            last: now,
            acceptance_until,
            round_until: 0,
            next_scan: now,
            budget: JobBudget::default(),
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
        self.scan_cache = ScanCache::default();
        self.cursor.stop();
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
        if !sender.sync_lane().sync_selected()
            || sender.sync_lane().sync_recursive_selected() != self.config.recursive
        {
            return Err(LiveError::NotNegotiated);
        }
        let context = sender.sync_lane().outgoing();
        let (direction, role, authority) = match self.direction {
            SyncDirection::HostToController => (
                fr_wire::files::Direction::ToController,
                fr_wire::files::Role::Host,
                fr_wire::files::Direction::ToHost,
            ),
            SyncDirection::ControllerToHost => (
                fr_wire::files::Direction::ToHost,
                fr_wire::files::Role::Controller,
                fr_wire::files::Direction::ToController,
            ),
            SyncDirection::Bidirectional => return Err(LiveError::WrongDirection),
        };
        if context.direction != direction || context.sender != role {
            return Err(LiveError::WrongDirection);
        }
        if self.authority.direction() != authority
            || self.authority.binding().session != context.session
            || self.authority.binding().lease != context.lease
            || !self.authority.matches_channel(context.channel)
            || !self.authority.matches_parent(sender.sync_lane().parent())
        {
            return Err(LiveError::WrongDirection);
        }
        let now = self.clock.now().as_nanos() / 1000;
        if now < self.last {
            return Err(LiveError::Clock);
        }
        self.last = now;
        self.authority
            .check(now)
            .map_err(|_| LiveError::Cancelled)?;
        sender.sync_lane().check(q).map_err(LiveError::Transport)?;
        if matches!(
            self.step,
            Step::Advertise | Step::Accept | Step::Advertisement | Step::BeginRound
        ) && now >= self.acceptance_until
        {
            return Err(LiveError::Expired);
        }
        if matches!(
            self.step,
            Step::StartScan
                | Step::Scanning
                | Step::StartTransfer
                | Step::PreparingTransfer
                | Step::DiscardTransfer
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
                    state(self.config.job_id, self.direction, SyncPhase::Advertised, 0),
                    Step::Accept,
                    now,
                )?;
            }
            Step::Accept => {
                if let Some(accepted) = self.receive_state(sender, q, authorize)? {
                    self.check_peer(accepted)?;
                    if accepted != state(self.config.job_id, self.direction, SyncPhase::Accepted, 0)
                    {
                        return Err(LiveError::WrongPhase);
                    }
                    self.start_round(now)?;
                }
            }
            Step::Advertisement => {
                if let Some(advertised) = self.receive_state(sender, q, authorize)? {
                    self.check_peer(advertised)?;
                    if advertised
                        != state(self.config.job_id, self.direction, SyncPhase::Advertised, 0)
                    {
                        return Err(LiveError::WrongPhase);
                    }
                    self.queue(
                        state(self.config.job_id, self.direction, SyncPhase::Accepted, 0),
                        Step::BeginRound,
                        now,
                    )?;
                }
            }
            Step::BeginRound => self.start_round(now)?,
            Step::StartScan => self.start_scan(now)?,
            Step::Scanning => self.poll_scan(now)?,
            Step::StartTransfer => {
                let prepared = self.prepared.as_mut().ok_or(LiveError::WrongPhase)?;
                // Cleanup may run before the first offer. Retain the exact
                // versions/report domain as soon as its source batch exists.
                self.versions = std::mem::take(&mut prepared.versions);
                self.reported = 0;
                self.recorded_ledger = false;
                sender
                    .begin_sync_preflight_before(
                        q,
                        std::mem::take(&mut prepared.selection),
                        self.round_until,
                        SourceScope::new(
                            self.authority.clone(),
                            self.permission.clone(),
                            self.stopped.clone(),
                        ),
                    )
                    .map_err(LiveError::Send)?;
                self.step = Step::PreparingTransfer;
            }
            Step::PreparingTransfer => self.prepare_transfer(sender, q, authorize, now)?,
            Step::DiscardTransfer => self.discard_transfer(sender, q)?,
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
        self.status.confirmed_conflicts = self
            .status
            .confirmed_conflicts
            .checked_add(settled.conflicts_kept)
            .ok_or(LiveError::Resource)?;
        self.status.state = settled;
        self.status.rounds_settled = self
            .status
            .rounds_settled
            .checked_add(1)
            .ok_or(LiveError::Resource)?;
        // Only this original peer settlement advances page fairness. A scan,
        // partial publication or source join alone never moves the cursor.
        let more = self.cursor.settle()?;
        self.versions.clear();
        self.next_scan = if more {
            // Deferred names retain no source descriptors. The next bounded
            // round revalidates the entire manifest under current authority.
            now
        } else {
            now.checked_add(self.config.interval_us)
                .ok_or(LiveError::Clock)?
        };
        self.step = Step::Waiting;
        Ok(())
    }

    fn start_scan(&mut self, now: u64) -> Result<(), LiveError> {
        if !self.scan_retry.begin(now, self.round_until)? {
            return Ok(());
        }
        self.scan = Some(Scan::start(
            &self.config,
            self.status.state.manifest_revision,
            self.ledger.clone(),
            self.cursor.after().map(str::to_owned),
            std::mem::take(&mut self.scan_cache),
            ScanScope {
                cx: self.cx.clone(),
                authority: self.authority.clone(),
                permission: self.permission.clone(),
                stopped: self.stopped.clone(),
                until: self.round_until,
            },
        )?);
        self.step = Step::Scanning;
        Ok(())
    }

    fn poll_scan(&mut self, now: u64) -> Result<(), LiveError> {
        let scanned = self.scan.as_mut().ok_or(LiveError::WrongPhase)?.poll();
        let mut prepared = match scanned {
            Ok(None) => return Ok(()),
            Ok(Some(prepared)) => prepared,
            Err(error) => {
                // poll() joins the old worker before yielding an error. Keep
                // the original Scanning revision while the source settles;
                // the controller is still waiting for its first manifest.
                self.scan = None;
                let retry_now = self.clock.now().as_nanos() / 1000;
                if retry_now < self.last {
                    return Err(LiveError::Clock);
                }
                self.last = retry_now;
                self.scan_retry.defer(error, retry_now, self.round_until)?;
                self.step = Step::StartScan;
                return Ok(());
            }
        };
        self.scan = None;
        let count = u32::try_from(prepared.versions.len()).map_err(|_| LiveError::Resource)?;
        self.budget.admit(count, prepared.bytes)?;
        // Retain verified scan metadata even when this round has no new versions.
        // Publication evidence remains exclusively in the confirmed ledger.
        self.scan_cache = std::mem::take(&mut prepared.cache);
        if count == 0 {
            self.cursor.stage(prepared.continuation)?;
            return self.announce_transfer(0, prepared.bytes, Step::Settlement, now);
        }
        // Keep the peer in its original Scanning/Manifest phase until the
        // first source independently verifies this selected version. No new
        // state or ATP offer is emitted if that source has already changed.
        self.prepared = Some(prepared);
        self.step = Step::StartTransfer;
        Ok(())
    }

    fn prepare_transfer(
        &mut self,
        sender: &mut Sender<'_>,
        q: &QuicRecords,
        authorize: &mut impl FnMut() -> bool,
        now: u64,
    ) -> Result<(), LiveError> {
        match sender.poll_sync_preflight(q, authorize) {
            Ok(false) => Ok(()),
            Ok(true) => {
                let prepared = self.prepared.take().ok_or(LiveError::WrongPhase)?;
                self.cursor.stage(prepared.continuation)?;
                let count = u32::try_from(self.versions.len()).map_err(|_| LiveError::Resource)?;
                self.announce_transfer(count, prepared.bytes, Step::Transfer, now)
            }
            Err(crate::sender::Error::SourceChanged) => {
                self.step = Step::DiscardTransfer;
                self.discard_transfer(sender, q)
            }
            Err(error) => Err(LiveError::Send(error)),
        }
    }

    fn discard_transfer(
        &mut self,
        sender: &mut Sender<'_>,
        q: &QuicRecords,
    ) -> Result<(), LiveError> {
        if !sender.discard_sync_preflight(q).map_err(LiveError::Send)? {
            return Ok(());
        }
        // The old worker is positively joined. Clear only unannounced scan
        // state; original authority, ledger, cursor, spent IDs and cumulative
        // transfer budgets remain unchanged.
        self.prepared = None;
        self.versions.clear();
        self.reported = 0;
        self.recorded_ledger = false;
        self.scan_cache = ScanCache::default();
        let retry_now = self.clock.now().as_nanos() / 1000;
        if retry_now < self.last {
            return Err(LiveError::Clock);
        }
        self.last = retry_now;
        self.scan_retry.defer(
            LiveError::Source(SyncError::SourceChanged),
            retry_now,
            self.round_until,
        )?;
        self.step = Step::StartScan;
        Ok(())
    }

    fn announce_transfer(
        &mut self,
        count: u32,
        bytes: u64,
        next: Step,
        now: u64,
    ) -> Result<(), LiveError> {
        let mut announcement = state(
            self.config.job_id,
            self.direction,
            SyncPhase::Transferring,
            self.status.state.manifest_revision,
        );
        announcement.entries_total = count;
        announcement.bytes_total = bytes;
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
        self.scan_retry = ScanRetry::default();
        self.step = Step::StartScan;
        self.queue(
            state(
                self.config.job_id,
                self.direction,
                SyncPhase::Scanning,
                revision,
            ),
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
            deadline: state_deadline(state.phase, now, self.acceptance_until, self.round_until)?,
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
            let ready = if matches!(pending.next, Step::Transfer) {
                sender.sync_preflight_ready(q)
            } else {
                sender.sync_ready(q)
            }
            .map_err(LiveError::Send)?;
            if !ready {
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
            let completed_at = self.clock.now().as_nanos() / 1000;
            if completed_at < self.last {
                return Err(LiveError::Clock);
            }
            if completed_at >= pending.deadline {
                return Err(LiveError::Expired);
            }
            self.last = completed_at;
            if matches!(pending.next, Step::Transfer) {
                sender
                    .release_sync_preflight(q)
                    .map_err(LiveError::Send)?;
            }
            self.step = pending.next;
            self.pending = None;
        }
        Ok(())
    }
    fn receive_state(
        &mut self,
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
        if let Some(error) = failure {
            return Err(error);
        }
        if received.is_some() {
            // Dispatch can wait or run a caller's authorization callback.
            // A late acceptance or settlement cannot advance this owner.
            let now = self.clock.now().as_nanos() / 1000;
            if now < self.last {
                return Err(LiveError::Clock);
            }
            self.last = now;
            match self.step {
                Step::Accept | Step::Advertisement if now >= self.acceptance_until => {
                    return Err(LiveError::Expired);
                }
                Step::Settlement if now >= self.round_until => return Err(LiveError::Expired),
                _ => {}
            }
        }
        Ok(received)
    }
    fn check_peer(&self, value: SyncJobState) -> Result<(), LiveError> {
        if value.job_id != self.config.job_id {
            return Err(LiveError::WrongJob);
        }
        if value.sync_direction != self.direction {
            return Err(LiveError::WrongDirection);
        }
        if matches!(value.phase, SyncPhase::Cancelled | SyncPhase::Failed) {
            return Err(LiveError::Peer(value.reason));
        }
        Ok(())
    }
    fn published_bytes(&self, outcome: Outcome) -> Option<u64> {
        match (self.direction, outcome) {
            (SyncDirection::HostToController, Outcome::ControllerPublished { bytes, .. })
            | (SyncDirection::ControllerToHost, Outcome::HostPublished { bytes, .. }) => {
                Some(bytes)
            }
            _ => None,
        }
    }

    fn observe_report(&mut self, sender: &Sender<'_>) -> Result<(), LiveError> {
        let Some(report) = sender.batch_report() else {
            return Ok(());
        };
        if report.total != self.versions.len() {
            return Err(LiveError::WrongPhase);
        }
        for (index, receipt) in report.receipts().enumerate().skip(self.reported) {
            if let Some(bytes) = self.published_bytes(receipt.outcome) {
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
                if self.published_bytes(receipt.outcome).is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_source_waits_before_a_second_scan_of_the_same_round() {
        let mut retry = ScanRetry::default();
        let until = 1_000_000;
        assert_eq!(retry.begin(10, until), Ok(true));
        retry
            .defer(LiveError::Source(SyncError::SourceChanged), 20, until)
            .unwrap();
        for now in [20, 50_000, 100_019] {
            assert_eq!(retry.begin(now, until), Ok(false));
            assert_eq!(retry.attempts, 1);
        }
        assert_eq!(retry.begin(100_020, until), Ok(true));
        assert_eq!(retry.attempts, 2);
    }

    #[test]
    fn repeated_edits_exhaust_three_scans_and_preserve_the_last_failure() {
        let mut retry = ScanRetry::default();
        let until = 1_000_000;
        let changed = LiveError::Source(SyncError::SourceChanged);
        for now in [0, 100_000] {
            assert_eq!(retry.begin(now, until), Ok(true));
            assert_eq!(retry.defer(changed, now, until), Ok(()));
        }
        assert_eq!(retry.begin(200_000, until), Ok(true));
        assert_eq!(retry.defer(changed, 200_000, until), Err(changed));
        assert_eq!(retry.attempts, 3);
        assert_eq!(retry.not_before, 200_000);
        assert_eq!(retry.begin(300_000, until), Err(changed));
        assert_eq!(retry.attempts, 3);
    }

    #[test]
    fn retry_never_extends_the_original_round_or_starts_at_its_deadline() {
        let changed = LiveError::Source(SyncError::SourceChanged);
        let mut retry = ScanRetry::default();
        assert_eq!(retry.begin(0, 200_000), Ok(true));
        retry.defer(changed, 0, 200_000).unwrap();
        assert_eq!(retry.begin(200_000, 200_000), Err(LiveError::Expired));
        assert_eq!(retry.attempts, 1);

        let mut retry = ScanRetry::default();
        assert_eq!(retry.begin(0, 100_000), Ok(true));
        assert_eq!(retry.defer(changed, 0, 100_000), Err(LiveError::Expired));
        assert_eq!(retry.not_before, 0);
    }

    #[test]
    fn authority_unsafe_sources_and_resource_failures_are_never_retried() {
        for error in [
            LiveError::Cancelled,
            LiveError::Expired,
            LiveError::Clock,
            LiveError::Panicked,
            LiveError::Worker,
            LiveError::Cleanup,
            LiveError::Resource,
            LiveError::Source(SyncError::AuthorityRevoked),
            LiveError::Source(SyncError::RootChanged),
            LiveError::Source(SyncError::SymlinkForbidden),
            LiveError::Source(SyncError::SubdirectoryUnsupported),
            LiveError::Source(SyncError::UnsupportedEntryKind),
            LiveError::Source(SyncError::EntryLimitExceeded),
            LiveError::Source(SyncError::FileSizeLimitExceeded),
            LiveError::Source(SyncError::TotalBytesLimitExceeded),
            LiveError::Source(SyncError::Io(std::io::ErrorKind::PermissionDenied)),
            LiveError::Source(SyncError::Io(std::io::ErrorKind::NotFound)),
            LiveError::Send(crate::sender::Error::SourceChanged),
        ] {
            let mut retry = ScanRetry::default();
            assert_eq!(retry.begin(0, 1_000_000), Ok(true));
            assert_eq!(retry.defer(error, 0, 1_000_000), Err(error));
            assert_eq!(retry.not_before, 0);
            assert_eq!(retry.attempts, 1);
        }
    }

    #[test]
    fn retry_timestamp_overflow_refuses_without_installing_a_wait() {
        let mut retry = ScanRetry::default();
        let now = u64::MAX - 1;
        assert_eq!(retry.begin(now, u64::MAX), Ok(true));
        assert_eq!(
            retry.defer(LiveError::Source(SyncError::SourceChanged), now, u64::MAX),
            Err(LiveError::Clock)
        );
        assert_eq!(retry.not_before, 0);
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "host/preflight_tests.rs"]
mod preflight_tests;
