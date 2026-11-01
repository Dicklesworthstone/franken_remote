//! Explicit local host sends on one original, already selected control session.
//! The socket is opt-in. Same-effective-user callers can select local paths;
//! remote peers can neither reach this endpoint nor choose host paths. A new
//! process or control session gets a new comparison instance and intake.
//!
//! Acknowledgements mean local selection admission. Publication, uncertainty
//! and cleanup remain separate, and up to 64 original result handles are kept
//! until this owner ends. Cancelling an old command cannot affect its successor.
#![forbid(unsafe_code)]

use super::{HostSendIntake, SelectReason, Selection, SendControl, SendPhase};
use fr_files::sender::{self, Outcome};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

mod ipc;
pub use ipc::{
    Cancellation, Cleanup, Error, Info, MAX_HISTORY, MAX_PATH_BYTES, MAX_PATHS,
    MAX_TOTAL_PATH_BYTES, Phase, SelectionFailure, Stage, Status, Submission, cancel, inspect,
    send, status, validate_target,
};

/// Owns only this local endpoint and its fixed workers. Drop/stop immediately
/// fences the original intake without joining a potentially stuck filesystem
/// call. `is_complete` remains false until BOTH original threads have exited.
pub struct Owner(ipc::Owner);
impl Owner {
    pub fn bind(target: &str, intake: HostSendIntake) -> Result<Self, Error> {
        let handler = Arc::new(Selections {
            intake,
            records: Mutex::new(Vec::with_capacity(MAX_HISTORY)),
        });
        ipc::Owner::bind(target, handler).map(Self)
    }
    pub fn info(&self) -> Result<Info, Error> {
        self.0.info()
    }
    pub fn stop(&self) {
        self.0.stop();
    }
    pub fn is_complete(&self) -> bool {
        self.0.is_complete()
    }
}

struct Record {
    control: SendControl,
    selected: u16,
}
struct Selections {
    intake: HostSendIntake,
    records: Mutex<Vec<Record>>,
}
impl Selections {
    fn records(&self) -> MutexGuard<'_, Vec<Record>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn snapshot(&self, records: &[Record], sequence: Option<u64>) -> Result<Status, Error> {
        let sequence =
            sequence.unwrap_or(u64::try_from(records.len()).map_err(|_| Error::Unavailable)?);
        let mut status = Status {
            sequence,
            ready: records.len() < MAX_HISTORY && self.intake.ready(),
            closed: self.intake.is_closed(),
            ..Status::default()
        };
        if sequence == 0 {
            return Ok(status);
        }
        let index = usize::try_from(sequence - 1).map_err(|_| Error::UnknownRequest)?;
        let record = records.get(index).ok_or(Error::UnknownRequest)?;
        let snapshot = record.control.snapshot();
        status.selected = record.selected;
        status.phase = match snapshot.status.phase {
            SendPhase::WaitingForControl => Phase::WaitingForControl,
            SendPhase::Negotiating => Phase::Negotiating,
            SendPhase::WaitingForSelection => Phase::WaitingForSelection,
            SendPhase::SelectionAccepted => Phase::SelectionAccepted,
            SendPhase::Sending => Phase::Sending,
            SendPhase::Finished => Phase::Finished,
            SendPhase::Ended => Phase::Ended,
        };
        status.stage = match snapshot.stage {
            None => Stage::Unknown,
            Some(sender::Stage::Idle) => Stage::Idle,
            Some(sender::Stage::Queued) => Stage::Queued,
            Some(sender::Stage::Preparing) => Stage::Preparing,
            Some(sender::Stage::AwaitingAcceptance) => Stage::AwaitingAcceptance,
            Some(sender::Stage::Streaming) => Stage::Streaming,
            Some(sender::Stage::AwaitingProof) => Stage::AwaitingProof,
            Some(sender::Stage::Finished) => Stage::Finished,
            Some(sender::Stage::Closed) => Stage::Closed,
        };
        status.failed = snapshot.status.absence.is_some();
        status.cancel_requested = snapshot.cancel_requested;
        status.queued_bytes = snapshot.current.map(|progress| progress.queued_bytes);
        status.total_bytes = snapshot.current.and_then(|progress| progress.total_bytes);
        status.cleanup = match snapshot.cleanup {
            None => Cleanup::Unknown,
            Some(Ok(())) => Cleanup::Confirmed,
            Some(Err(_)) => Cleanup::Failed,
        };
        if let Some(report) = snapshot.status.report {
            status.started = u16::try_from(report.started).map_err(|_| Error::Unavailable)?;
            status.complete = report.complete;
            status.failed |= report.stop.is_some();
            for receipt in report.receipts() {
                match receipt.outcome {
                    Outcome::HostPublished { publication, .. }
                    | Outcome::ControllerPublished { publication, .. } => {
                        status.published += 1;
                        if publication == super::Publication::DurabilityUnknown {
                            status.durability_unknown += 1;
                        }
                    }
                    Outcome::HostRefused(_) | Outcome::ControllerRefused(_) => status.refused += 1,
                    Outcome::InterruptedBeforePublication(_) => status.interrupted += 1,
                    Outcome::PublicationUnknown => status.publication_unknown += 1,
                }
            }
        }
        Ok(status)
    }
}
impl ipc::Handler for Selections {
    fn status(&self, sequence: Option<u64>) -> Result<Status, Error> {
        if sequence == Some(0) {
            return Err(Error::InvalidRequest);
        }
        self.snapshot(&self.records(), sequence)
    }
    fn select(
        &self,
        paths: &[PathBuf],
        until: Instant,
        stopped: &AtomicBool,
    ) -> Result<Status, Error> {
        let check = || {
            if stopped.load(Ordering::Acquire) || self.intake.is_closed() {
                return Err(Error::Closed);
            }
            if Instant::now() >= until {
                return Err(Error::Timeout);
            }
            if self.records().len() == MAX_HISTORY {
                return Err(Error::Exhausted);
            }
            if !self.intake.ready() {
                return Err(Error::Busy);
            }
            Ok(())
        };
        check()?;
        // Only this fixed local selection worker touches paths. No authority,
        // history or socket mutex is held across filesystem work.
        let request = Selection::select(paths)
            .and_then(|selection| selection.request())
            .map_err(|refusal| Error::Selection {
                index: u8::try_from(refusal.index).unwrap_or(u8::MAX),
                reason: match refusal.reason {
                    SelectReason::TooMany => SelectionFailure::TooMany,
                    SelectReason::Missing => SelectionFailure::Missing,
                    SelectReason::Symlink => SelectionFailure::Symlink,
                    SelectReason::Directory => SelectionFailure::Directory,
                    SelectReason::SpecialFile => SelectionFailure::SpecialFile,
                    SelectReason::NotPortable => SelectionFailure::NotPortable,
                    SelectReason::DuplicateName => SelectionFailure::DuplicateName,
                    SelectReason::Unreadable => SelectionFailure::Unreadable,
                    SelectReason::Changed => SelectionFailure::Changed,
                },
            })?;
        check()?;
        let mut records = self.records();
        // A status reader may have held this bounded history lock. Sampling
        // before waiting for it must not renew the original command budget.
        if stopped.load(Ordering::Acquire) || self.intake.is_closed() {
            return Err(Error::Closed);
        }
        if Instant::now() >= until {
            return Err(Error::Timeout);
        }
        let control = self.intake.submit(request).map_err(|error| match error {
            sender::Error::Busy => Error::Busy,
            sender::Error::Closed => Error::Closed,
            sender::Error::Limits => Error::InvalidPaths,
            _ => Error::Unavailable,
        })?;
        records.push(Record {
            control,
            selected: u16::try_from(paths.len()).map_err(|_| Error::InvalidPaths)?,
        });
        self.snapshot(&records, None)
    }
    fn cancel(&self, sequence: u64) -> Result<(Status, bool), Error> {
        let records = self.records();
        let index = usize::try_from(sequence.checked_sub(1).ok_or(Error::InvalidRequest)?)
            .map_err(|_| Error::UnknownRequest)?;
        let record = records.get(index).ok_or(Error::UnknownRequest)?;
        let requested = record.control.cancel();
        Ok((self.snapshot(&records, Some(sequence))?, requested))
    }
    fn close(&self) {
        self.intake.close();
    }
}

#[cfg(test)]
mod tests;
