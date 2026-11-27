//! Exclusive job-state custody between complete ATP batches.
use super::{Error, Sender, batch};
use crate::session::{Authority, Permission};
use fr_transport::quic::{QuicRecords, files::FilesChannel};
use fr_wire::files::{Direction, Role};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// The source job's original consent and control lifetime. Cloning this handoff
/// cannot renew authority, approve another source, or reopen a stopped job.
#[derive(Clone)]
pub(crate) struct SourceScope {
    authority: Authority,
    permission: Permission,
    stopped: Arc<AtomicBool>,
}
impl SourceScope {
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

    fn check_lane(&self, lane: &FilesChannel) -> Result<(), Error> {
        let context = lane.outgoing();
        let (direction, role) = match self.authority.direction() {
            Direction::ToHost => (Direction::ToController, Role::Host),
            Direction::ToController => (Direction::ToHost, Role::Controller),
        };
        if !lane.sync_selected()
            || context.direction != direction
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

impl Sender<'_> {
    /// Retain the same source authority through independent hashing and every
    /// later payload read, including while the parent reactor is not serviced.
    /// Ordinary local selections keep their existing authorization contract.
    pub(crate) fn begin_sync_batch_before(
        &mut self,
        q: &QuicRecords,
        selection: batch::Selection,
        deadline: u64,
        scope: SourceScope,
    ) -> Result<(), Error> {
        self.identity(q)?;
        scope.check_lane(&self.lane)?;
        scope.check(self.now())?;
        self.begin_scoped_batch_before(q, selection, deadline, Some(scope))
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
