//! Explicit local multi-selection on the original sender, never a sync job.
//! One source thread and one transfer at a time; no new wire profile or channel.
use super::{Error, Outcome, Receipt, Sender, Stage, micros};
use crate::receive::Publication;
use asupersync::atp::safety::validate_portable_path_component;
use fr_transport::quic::QuicRecords;
use fr_wire::files::Reason;
use std::{collections::VecDeque, fmt, fs::File, time::Duration};

/// Bounds descriptors, basenames, retained receipts and work per selection.
pub const MAX_FILES: usize = 32;
const MAX_NAME_BYTES: usize = 255;

struct Selected {
    file: File,
    name: String,
}
/// Only already selected local descriptors can enter a batch. Construction does
/// not inspect, hash or read them. Names are destination basenames, never paths.
#[derive(Default)]
pub struct Selection {
    files: VecDeque<Selected>,
}
impl fmt::Debug for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSelection")
            .field("files", &self.len())
            .finish_non_exhaustive()
    }
}
impl Selection {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.files.len()
    }
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    /// Duplicate basenames refuse before ANY source starts. This never renames
    /// files or chooses a conflict policy for the user. The receiver still owns
    /// case/normalization conflicts and its no-overwrite publication rule.
    pub fn push(&mut self, file: File, name: &str) -> Result<(), Error> {
        if self.files.len() == MAX_FILES {
            return Err(Error::Limits);
        }
        if name.len() > MAX_NAME_BYTES
            || name.starts_with(".fr-part-")
            || validate_portable_path_component(name).is_err()
            || self.files.iter().any(|selected| selected.name == name)
        {
            return Err(Error::Name);
        }
        let mut owned = String::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| Error::Limits)?;
        owned.push_str(name);
        self.files.try_reserve_exact(1).map_err(|_| Error::Limits)?;
        self.files.push_back(Selected { file, name: owned });
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Local(Error),
    HostRefused(Reason),
    /// Publication may have happened. This selection is NEVER resubmitted.
    PublicationUnknown,
    /// Publication is known, durability is not. Keep that original receipt.
    DurabilityUnknown,
    Cleanup(Error),
}
/// Content-free snapshot in selection order. A receipt always belongs to an
/// actually started transfer. `not_started` is not a rollback of earlier files.
/// `complete` additionally requires the original source's successful cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub total: usize,
    pub started: usize,
    pub complete: bool,
    pub stop: Option<Stop>,
    receipts: [Option<Receipt>; MAX_FILES],
    received: usize,
}
impl Report {
    pub fn receipts(&self) -> impl ExactSizeIterator<Item = Receipt> + '_ {
        self.receipts[..self.received]
            .iter()
            .map(|r| r.expect("only recorded prefix exposed"))
    }
    /// No source thread was started for these selections. Before stop/complete
    /// they are still queued, not a claim that the operation has ended.
    pub fn not_started(&self) -> usize {
        self.total - self.started
    }
}

pub(super) struct Batch {
    selection: Selection,
    deadline: u64,
    last: u64,
    report: Report,
    recorded_current: bool,
}
impl Batch {
    pub(super) fn report_complete(&self) -> bool {
        self.report.complete
    }
    pub(super) fn no_pending_sources(&self) -> bool {
        self.selection.is_empty()
    }
    pub(super) fn stop(&mut self, reason: Stop) {
        if self.report.complete {
            return;
        }
        self.report.stop.get_or_insert(reason);
        self.selection.files.clear();
    }
}
impl Sender<'_> {
    /// Own a finite user selection without starting a disk operation. The one
    /// absolute deadline starts NOW, covering queued files, hashing, transfers,
    /// proof waits and inter-file cleanup. Each source is also capped by the
    /// existing per-file lifetime. Renewal never buys additional batch time.
    /// Only `service` can start a source, after its current authorization check.
    pub fn begin_batch(
        &mut self,
        q: &QuicRecords,
        selection: Selection,
        lifetime: Duration,
    ) -> Result<(), Error> {
        self.identity(q)?;
        if self.closed {
            return Err(Error::Closed);
        }
        self.lane.check(q).map_err(Error::Transport)?;
        if self.transfer.is_some() || self.batch.is_some() {
            return Err(Error::Busy);
        }
        let total = selection.len();
        if total == 0 || !(Duration::from_micros(1)..=Duration::from_secs(3600)).contains(&lifetime)
        {
            return Err(Error::Limits);
        }
        self.next.checked_add(total as u64).ok_or(Error::Limits)?;
        self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let deadline = self
            .now()
            .checked_add(micros(lifetime)?)
            .ok_or(Error::Clock)?;
        self.batch = Some(Batch {
            selection,
            deadline,
            last: self.now(),
            report: Report {
                total,
                started: 0,
                complete: false,
                stop: None,
                receipts: [None; MAX_FILES],
                received: 0,
            },
            recorded_current: false,
        });
        Ok(())
    }
    /// Readable during transfer, after cancellation and after parent closure.
    /// Earlier durable receipts never disappear behind a later file's failure.
    pub fn batch_report(&self) -> Option<Report> {
        self.batch.as_ref().map(|b| b.report)
    }
    /// A new batch/single file is blocked until this report is collected AND the
    /// original source cleanup succeeded. Failure never reopens a retired lane.
    pub fn take_batch_report(&mut self) -> Option<Report> {
        if !self.batch.as_ref()?.report.complete {
            return None;
        }
        self.batch.take().map(|b| b.report)
    }
    pub(super) fn service_batch(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<Stage, Error> {
        let batch = self.batch.as_ref().expect("batch service");
        if batch.report.complete {
            return Ok(Stage::Finished);
        }
        if batch.report.stop.is_none() {
            let now = self.now();
            let error = if now < batch.last {
                Some(Error::Clock)
            } else if self.closed {
                Some(Error::Closed)
            } else if self.cx.is_cancel_requested() || !authorize() {
                Some(Error::Cancelled)
            } else if now >= batch.deadline {
                Some(Error::Expired)
            } else {
                None
            };
            if let Some(error) = error {
                self.fail(error);
                let _ = self.retire(q);
                self.settle_batch();
                return Err(error);
            }
            self.batch.as_mut().expect("original batch retained").last = now;
            if self.transfer.is_none() {
                self.start_selected(q)?;
            }
        }
        let result = self.service_one(q, authorize);
        self.settle_batch();
        result.map(|_| self.stage())
    }
    fn start_selected(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        let batch = self.batch.as_mut().expect("batch service");
        let deadline = batch.deadline;
        let Some(selected) = batch.selection.files.pop_front() else {
            return Ok(());
        };
        if let Err(error) = self.begin_until(q, selected.file, &selected.name, Some(deadline)) {
            self.fail(error);
            let _ = self.retire(q);
            self.settle_batch();
            return Err(error);
        }
        let batch = self.batch.as_mut().expect("original batch retained");
        batch.report.started += 1;
        batch.recorded_current = false;
        Ok(())
    }
    /// Poll a finished source only. This is also called by offline cleanup, so
    /// losing the connection cannot lose the final file's effect or old receipts.
    pub(super) fn settle_batch(&mut self) {
        let Some(batch) = &mut self.batch else { return };
        if let Some(t) = &mut self.transfer {
            let Some(receipt) = t.result else { return };
            if !batch.recorded_current {
                batch.report.receipts[batch.report.received] = Some(receipt);
                batch.report.received += 1;
                batch.recorded_current = true;
                let stop = match receipt.outcome {
                    Outcome::HostPublished {
                        publication: Publication::Durable,
                        ..
                    } => None,
                    Outcome::HostPublished { .. } => Some(Stop::DurabilityUnknown),
                    Outcome::HostRefused(reason) => Some(Stop::HostRefused(reason)),
                    Outcome::PublicationUnknown => Some(Stop::PublicationUnknown),
                    Outcome::InterruptedBeforePublication(error) => Some(Stop::Local(error)),
                };
                if let Some(stop) = stop {
                    batch.stop(stop);
                }
            }
            if t.cleanup.is_none() {
                t.cleanup = t.source.reap();
            }
            match t.cleanup {
                None => return,
                Some(Err(error)) => {
                    batch.stop(Stop::Cleanup(error));
                    return;
                }
                Some(Ok(())) => self.transfer = None,
            }
        }
        batch.report.complete = batch.selection.is_empty();
    }
}
