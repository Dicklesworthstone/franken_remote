//! Exclusive job-state custody between complete ATP batches.
use super::{Error, Sender};
use fr_transport::quic::{QuicRecords, files::FilesChannel};

impl Sender<'_> {
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
