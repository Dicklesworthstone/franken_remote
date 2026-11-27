//! Exclusive job-state custody around bounded ATP batches and preparation.
use super::{Error, Event, Sender, Stage, Transfer, batch};
use crate::session::{Authority, Permission};
use fr_transport::quic::{QuicRecords, files::FilesChannel};
use fr_wire::files::{Direction, Role};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// An original sender or source job's consent and control lifetime. Cloning
/// this handoff cannot renew authority or reopen a stopped owner.
#[derive(Clone)]
pub(crate) struct SourceScope {
    authority: Authority,
    permission: Permission,
    stopped: Arc<AtomicBool>,
}
impl SourceScope {
    pub(super) fn for_sender(authority: Authority, permission: Permission) -> Self {
        Self::new(authority, permission, Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn new(
        authority: Authority,
        permission: Permission,
        stopped: Arc<AtomicBool>,
    ) -> Self {
        Self {
            authority,
            permission,
            stopped,
        }
    }

    pub(super) fn check(&self, now: u64) -> Result<(), Error> {
        if self.stopped.load(Ordering::Acquire) || !self.permission.is_approved() {
            return Err(Error::Cancelled);
        }
        self.authority.check(now).map_err(|_| Error::Cancelled)?;
        // Projection checks can take a policy lock. Local source consent may
        // be revoked independently while that original check is in progress.
        if self.stopped.load(Ordering::Acquire) || !self.permission.is_approved() {
            return Err(Error::Cancelled);
        }
        Ok(())
    }

    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    pub(super) fn check_lane(&self, lane: &FilesChannel) -> Result<(), Error> {
        let context = lane.outgoing();
        let (direction, role) = match self.authority.direction() {
            Direction::ToHost => (Direction::ToController, Role::Host),
            Direction::ToController => (Direction::ToHost, Role::Controller),
        };
        if context.direction != direction
            || context.sender != role
            || self.authority.binding().session != context.session
            || self.authority.binding().lease != context.lease
            || !self.authority.matches_channel(context.channel)
            || !self.authority.matches_parent(lane.parent())
        {
            return Err(Error::WrongRole);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Preparation {
    Preparing,
    Ready,
    Changed,
    Discarding,
}

/// No ATP admission is possible until the job owner releases this hold after
/// its original Transferring state record has drained. Transfer IDs and the
/// batch's absolute deadline are already spent and are never rolled back.
pub(super) struct Preflight {
    phase: Preparation,
    deadline: u64,
    last: u64,
}

impl Sender<'_> {
    /// Hold one bounded folder selection under its original authority/deadline.
    /// Only the first source may prepare, with no offer, ATP packet or receipt.
    /// The job owner announces the verified manifest before releasing this hold.
    pub(crate) fn begin_sync_preflight_before(
        &mut self,
        q: &QuicRecords,
        selection: batch::Selection,
        deadline: u64,
        scope: SourceScope,
    ) -> Result<(), Error> {
        self.identity(q)?;
        if !self.lane.sync_selected() {
            return Err(Error::WrongRole);
        }
        scope.check_lane(&self.lane)?;
        scope.check(self.now())?;
        if !self.sync_ready(q)? {
            return Err(Error::Busy);
        }
        self.begin_scoped_batch_before(q, selection, deadline, Some(scope))?;
        self.sync_preflight = Some(Preflight {
            phase: Preparation::Preparing,
            deadline,
            last: self.now(),
        });
        Ok(())
    }

    /// Start/poll only the first source. Retain its actual Prepared event so
    /// normal service hashes nothing again and gives the offer a fresh record
    /// deadline only after the job announcement has drained.
    pub(crate) fn poll_sync_preflight(
        &mut self,
        q: &QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        self.check_preflight(q)?;
        if !authorize() {
            return Err(Error::Cancelled);
        }
        // A callback can block or revoke consent. Inspect both immutable
        // scopes again at a fresh time before starting or accepting disk work.
        self.check_preflight(q)?;
        match self.sync_preflight.as_ref().expect("checked hold").phase {
            Preparation::Ready => return Ok(true),
            Preparation::Changed | Preparation::Discarding => return Err(Error::SourceChanged),
            Preparation::Preparing => {}
        }
        if self.transfer.is_none() {
            if self.paused {
                return Ok(false);
            }
            self.start_selected(q)?;
        }
        self.check_preflight(q)?;
        let transfer = self.transfer.as_mut().ok_or(Error::Protocol)?;
        match poll_prepared(transfer) {
            Ok(false) => Ok(false),
            Ok(true) => {
                self.sync_preflight.as_mut().expect("checked hold").phase = Preparation::Ready;
                Ok(true)
            }
            Err(Error::SourceChanged) => {
                // Stop only this unpublished source, never the standing job's
                // or sender's scope. The job must positively join it below.
                transfer.source.stop();
                transfer.ready = None;
                self.sync_preflight.as_mut().expect("checked hold").phase = Preparation::Changed;
                Err(Error::SourceChanged)
            }
            Err(error) => Err(error),
        }
    }

    /// Discard only a known changed, never-offered first source. No successor
    /// can start until its actual worker has joined successfully. Cleanup is
    /// nonblocking and still bounded by the same original round deadline.
    pub(crate) fn discard_sync_preflight(&mut self, q: &QuicRecords) -> Result<bool, Error> {
        self.check_preflight(q)?;
        let preflight = self.sync_preflight.as_mut().expect("checked hold");
        if !matches!(preflight.phase, Preparation::Changed | Preparation::Discarding) {
            return Err(Error::Protocol);
        }
        preflight.phase = Preparation::Discarding;
        let transfer = self.transfer.as_mut().ok_or(Error::Protocol)?;
        transfer.source.stop();
        if transfer.cleanup.is_none() {
            transfer.cleanup = transfer.source.reap();
        }
        match transfer.cleanup {
            None => return Ok(false),
            Some(Err(error)) => return Err(error),
            Some(Ok(())) => {}
        }
        // Joining can consume time. Expiry/revocation remains terminal and
        // retains this completed cleanup for the ordinary cancellation path.
        self.check_preflight(q)?;
        self.transfer = None;
        self.batch = None;
        self.sync_preflight = None;
        Ok(true)
    }

    /// Job-state admission with exactly one held first Prepared event. This
    /// deliberately preserves [`Self::sync_ready`]'s complete-batch requirement.
    /// While the state record is retained, neither this method nor ATP service
    /// may reconcile the channel's retained-record count.
    pub(crate) fn sync_preflight_ready(&mut self, q: &QuicRecords) -> Result<bool, Error> {
        self.check_preflight(q)?;
        Ok(self.sync_preflight.as_ref().expect("checked hold").phase == Preparation::Ready
            && self.lane.send_drained(q).map_err(Error::Transport)?)
    }

    /// The unchanged first Transferring announcement must already be drained.
    /// Only then may ordinary service consume Prepared and encode its one offer.
    pub(crate) fn release_sync_preflight(&mut self, q: &QuicRecords) -> Result<(), Error> {
        if !self.sync_preflight_ready(q)? {
            return Err(Error::Busy);
        }
        self.sync_preflight = None;
        Ok(())
    }

    fn check_preflight(&mut self, q: &QuicRecords) -> Result<(), Error> {
        self.identity(q)?;
        if self.closed {
            return Err(Error::Closed);
        }
        self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
        self.lane.check(q).map_err(Error::Transport)?;
        if !self.lane.sync_selected() {
            return Err(Error::WrongRole);
        }
        let now = self.now();
        let preflight = self.sync_preflight.as_ref().ok_or(Error::Protocol)?;
        if now < preflight.last {
            return Err(Error::Clock);
        }
        if now >= preflight.deadline {
            return Err(Error::Expired);
        }
        if let Some(scope) = &self.source_scope {
            scope.check(now)?;
        }
        let batch = self.batch.as_ref().ok_or(Error::Protocol)?;
        batch.source_scope().ok_or(Error::Protocol)?.check(now)?;
        let report = self.batch_report().ok_or(Error::Protocol)?;
        if report.total == 0
            || report.started > 1
            || report.complete
            || report.stop.is_some()
            || report.receipts().next().is_some()
            || !self.flight.is_empty()
            || self.lane.retained_send_records(q).map_err(Error::Transport)? != 0
        {
            return Err(Error::Protocol);
        }
        match &self.transfer {
            Some(transfer)
                if report.started == 1
                    && transfer.stage == Stage::Preparing
                    && transfer.result.is_none()
                    && transfer.packet.is_none()
                    && !transfer.completion_admitted
                    && transfer.queued == 0
                    && transfer.total.is_none()
                    && transfer.entries.is_empty()
                    && transfer.root.is_empty()
                    && matches!(&transfer.ready, None | Some(Event::Prepared(_))) =>
            {
                if now >= transfer.deadline {
                    return Err(Error::Expired);
                }
                if preflight.phase == Preparation::Ready
                    && !matches!(&transfer.ready, Some(Event::Prepared(_)))
                {
                    return Err(Error::Protocol);
                }
            }
            None if report.started == 0 && preflight.phase == Preparation::Preparing => {}
            _ => return Err(Error::Protocol),
        }
        let checked_at = self.now();
        if checked_at < now {
            return Err(Error::Clock);
        }
        if checked_at >= preflight.deadline
            || self
                .transfer
                .as_ref()
                .is_some_and(|transfer| checked_at >= transfer.deadline)
        {
            return Err(Error::Expired);
        }
        self.sync_preflight.as_mut().expect("checked hold").last = checked_at;
        Ok(())
    }

    pub(crate) fn sync_lane(&self) -> &FilesChannel {
        &self.lane
    }
    /// Reconcile only ATP-owned records, BEFORE admitting a job-state record.
    /// While a job state is retained, its separate owner checks lane drain and
    /// must not call this method or ordinary sender service. No record is
    /// excluded from the transport count or allowed to overlap these owners.
    pub(crate) fn sync_ready(&mut self, q: &QuicRecords) -> Result<bool, Error> {
        self.identity(q)?;
        self.lane.check(q).map_err(Error::Transport)?;
        if self.closed {
            return Err(Error::Closed);
        }
        if !self.lane.sync_selected() {
            return Err(Error::WrongRole);
        }
        if self.transfer.is_some() || self.batch.is_some() {
            return Ok(false);
        }
        let retained = self
            .lane
            .retained_send_records(q)
            .map_err(Error::Transport)?;
        if !self.flight.reconcile(retained) {
            return Err(Error::Protocol);
        }
        if self.flight.expired(self.now()) {
            return Err(Error::Expired);
        }
        Ok(self.flight.is_empty() && self.lane.send_drained(q).map_err(Error::Transport)?)
    }
}

fn poll_prepared(transfer: &mut Transfer) -> Result<bool, Error> {
    if transfer.ready.is_none() {
        transfer.ready = transfer.source.poll()?;
    }
    let manifest = match &transfer.ready {
        None => return Ok(false),
        Some(Event::Prepared(manifest)) => manifest,
        Some(_) => return Err(Error::Protocol),
    };
    // Do not call Transfer::prepared here: it selects the directory-entry ATP
    // profile and must run exactly once when the offer is actually encoded.
    if manifest.is_directory
        != (transfer.profile == fr_wire::files::ATP_PORTABLE_DIRECTORY_FULL)
    {
        return Err(Error::Protocol);
    }
    #[cfg(unix)]
    {
        let expected = transfer.expected_source.as_ref().ok_or(Error::Protocol)?;
        if !expected.matches(manifest) {
            return Err(Error::SourceChanged);
        }
    }
    Ok(true)
}
