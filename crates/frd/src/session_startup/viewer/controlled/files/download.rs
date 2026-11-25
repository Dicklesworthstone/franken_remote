//! A client destination belongs to the original decoder-backed controller.
//! Each actual terminal disk receipt is retained before a later turn can replace
//! the receiver's last-result slot. Cleanup never guesses publication or replay.
use super::{ControlledViewer, Error};
use crate::native_files::{Absence, ReceiveControl, ReceivePhase, ReceiveStatus};
use asupersync::cx::Cx;
use fr_files::{
    quic::{self, Configuration, Receiver, State},
    sync::{ControllerJob, JobStatus, LiveError},
    wire::ResultReceipt,
    worker,
};
use fr_transport::quic::QuicRecords;
use fr_wire::sync_job_state::JobMode;
use std::time::Duration;

pub(super) fn send_error(error: quic::Error) -> Error {
    match error {
        quic::Error::WrongConnection => Error::WrongConnection,
        quic::Error::WrongRole => Error::WrongRole,
        quic::Error::Limits => Error::Limits,
        quic::Error::Clock => Error::Clock,
        quic::Error::Cancelled => Error::Cancelled,
        quic::Error::Transport(error) => Error::Transport(error),
        quic::Error::File(_) => Error::Protocol,
    }
}

fn receive_sync_error(error: LiveError) -> quic::Error {
    match error {
        LiveError::Receive(error) => error,
        LiveError::Transport(error) => quic::Error::Transport(error),
        LiveError::WrongConnection => quic::Error::WrongConnection,
        LiveError::Clock => quic::Error::Clock,
        LiveError::Cancelled => quic::Error::Cancelled,
        _ => quic::Error::File(fr_files::wire::Error::Wire(
            fr_wire::WireError::InvalidValue,
        )),
    }
}

pub(super) struct Receiving {
    pub(super) receiver: Receiver,
    control: ReceiveControl,
    failure: Option<quic::Error>,
    cleanup: Option<Result<(), worker::Error>>,
    sync: Option<Box<ControllerJob>>,
}
impl Receiving {
    pub(super) fn new(
        receiver: Receiver,
        control: ReceiveControl,
        sync: Option<Box<ControllerJob>>,
    ) -> Self {
        if let Some(job) = &sync {
            control.record_folder_job(job.status());
        }
        Self {
            receiver,
            control,
            failure: None,
            cleanup: None,
            sync,
        }
    }
    pub(super) fn observe(&self) {
        if let Some(job) = &self.sync {
            self.control.record_folder_job(job.status());
        }
        self.control.progress(self.receiver.progress());
        if let Some(receipt) = self.receiver.last_result() {
            self.control.observe(receipt);
        }
    }
    pub(super) fn service(
        &mut self,
        q: &mut QuicRecords,
        authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.observe();
        if self.control.cancel_requested() {
            if let Some(sync) = &mut self.sync {
                sync.stop();
            }
            // As in `stop`: a cancelled receive has ended even if its reset fails.
            let retired = self.receiver.retire(q).map_err(send_error);
            self.control.set_phase(ReceivePhase::Ended);
            let _ = self.collect();
            return retired;
        }
        let result = if let Some(sync) = &mut self.sync {
            sync.service(&mut self.receiver, q, authorize)
                .map_err(receive_sync_error)
        } else {
            self.receiver.service(q, authorize).map(|_| ())
        };
        if let Err(error) = result {
            self.failure.get_or_insert(error);
            self.control.end(Absence::ReceiveFailed(error));
            self.observe();
            if error == quic::Error::WrongConnection || q.is_closed() {
                return Err(send_error(error));
            }
        }
        self.observe();
        if self.receiver.state() != State::Active {
            let _ = self.collect();
        }
        Ok(())
    }
    pub(super) fn stop(&mut self, q: &mut QuicRecords, cleanup: Option<&Cx>) -> Result<(), Error> {
        self.observe();
        if let Some(sync) = &mut self.sync {
            sync.stop();
        }
        self.receiver.stop();
        // An orderly close must not add a reset to an idle pair: its terminal
        // exchange requires an empty sender. In-flight publication is fenced
        // now; actual disk completion is collected after the parent closes.
        let retired = match cleanup {
            None => self.receiver.retire(q).map_err(send_error),
            Some(_) => Ok(()),
        };
        // `receiver.stop` already fenced publication, so this receive has
        // ended even when its pair reset fails (a failed reset closes the
        // connection). Report that before returning the reset's error.
        self.control.set_phase(ReceivePhase::Ended);
        let _ = self.collect();
        retired
    }
    pub(super) fn collect(&mut self) -> Option<Result<(), worker::Error>> {
        if let Err(error) = self.receiver.collect_after_close() {
            self.failure.get_or_insert(error);
            self.control.end(Absence::ReceiveFailed(error));
        }
        self.observe();
        if self.cleanup.is_none() {
            self.cleanup = self.receiver.try_finish_cleanup();
            if self.cleanup.is_some() {
                // A publication can finish between the preceding poll and join.
                if let Err(error) = self.receiver.collect_after_close() {
                    self.failure.get_or_insert(error);
                    self.control.end(Absence::ReceiveFailed(error));
                }
                self.observe();
            }
        }
        if let Some(sync) = &mut self.sync {
            // A final publication may race the local stop. Preserve the exact
            // original receipt without sending a late round acknowledgement.
            let _ = sync.collect_after_close(&self.receiver);
        }
        if let Some(result) = self.cleanup {
            if let Some(sync) = &mut self.sync {
                sync.cleaned(result);
            }
            self.control.cleaned(result);
        }
        if let Some(job) = &self.sync {
            self.control.record_folder_job(job.status());
        }
        self.cleanup
    }
}

impl ControlledViewer {
    /// Receive a host's explicit selection into an already approved local
    /// directory. Observers, legacy upload-only negotiation and a second lane
    /// refuse. No disk worker exists before the bounded attachment completes.
    pub fn expect_file_download(
        &mut self,
        configuration: Configuration,
        timeout: Duration,
    ) -> Result<(), Error> {
        self.expect_file_download_with_control(configuration, ReceiveControl::new(), timeout)
    }
    pub(in crate::session_startup) fn expect_file_download_with_control(
        &mut self,
        configuration: Configuration,
        control: ReceiveControl,
        timeout: Duration,
    ) -> Result<(), Error> {
        self.expect_file_download_with_sync(configuration, control, None, timeout)
    }
    pub(in crate::session_startup) fn expect_file_download_with_sync(
        &mut self,
        mut configuration: Configuration,
        control: ReceiveControl,
        sync_job: Option<(u64, JobMode)>,
        timeout: Duration,
    ) -> Result<(), Error> {
        crate::session_startup::native_control::file_download_lane(&self.session.opened.selection)
            .map_err(|_| Error::WrongRole)?;
        let sync = if let Some((job_id, mode)) = sync_job {
            if !mode.download_selected(&self.session.opened.selection) {
                return Err(Error::WrongRole);
            }
            let job = ControllerJob::new(job_id, configuration.directory.clone())
                .map_err(|error| send_error(receive_sync_error(error)))?
                .with_mode(mode);
            configuration.directory = job.directory();
            Some(Box::new(job))
        } else {
            None
        };
        let permission = configuration.permission.clone();
        self.expect_file_target(
            None,
            permission,
            super::negotiation::Destination::Receive(configuration, control, sync),
            timeout,
        )
    }
    pub fn file_receive_state(&self) -> Option<State> {
        self.files
            .download
            .receiver
            .as_ref()
            .map(|r| r.receiver.state())
    }
    pub fn file_receive_progress(&self) -> Option<fr_files::session::Progress> {
        self.files
            .download
            .receiver
            .as_ref()
            .and_then(|r| r.receiver.progress())
    }
    pub fn file_receive_result(&self) -> Option<ResultReceipt> {
        self.files
            .download
            .receiver
            .as_ref()
            .and_then(|r| r.receiver.last_result())
    }
    pub fn file_receive_status(&self) -> Option<ReceiveStatus> {
        self.files
            .download
            .receiver
            .as_ref()
            .map(|r| r.control.status())
    }
    /// Content-free progress of the explicitly accepted original job, distinct
    /// from individual file receipts and joined disk-cleanup evidence.
    pub fn file_sync_status(&self) -> Option<JobStatus> {
        self.files
            .download
            .receiver
            .as_ref()?
            .sync
            .as_ref()
            .map(|job| job.status())
    }
    pub fn file_receive_failure(&self) -> Option<quic::Error> {
        self.files
            .download
            .receiver
            .as_ref()
            .and_then(|r| r.failure)
    }
    /// Setup/attachment failure for this receive owner only; an unrelated
    /// upload failure cannot change this direction's status or cancellation.
    pub fn file_receive_setup_failure(&self) -> Option<Error> {
        self.files.download.failure
    }
    pub fn file_receive_negotiating(&self) -> bool {
        self.files
            .download
            .pending
            .as_ref()
            .is_some_and(super::negotiation::Pending::is_download)
    }
    /// Nonblocking original-worker collection, including after parent closure.
    /// The actual result also updates the retained native `ReceiveControl`.
    pub fn try_finish_file_receive_cleanup(&mut self) -> Option<Result<(), worker::Error>> {
        let receiving = self.files.download.receiver.as_mut()?;
        if receiving.receiver.state() == State::Active {
            return None;
        }
        receiving.collect()
    }
}
