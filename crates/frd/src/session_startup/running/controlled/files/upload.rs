//! A host-selected destination advertises one reverse folder job.
use super::{Configuration, ControlledHost, Error, negotiation::Target};
use fr_files::{
    quic,
    sync::{JobStatus, LiveError, ReceiverJob},
};
use fr_transport::quic::ChannelRequest;
use fr_wire::sync_job_state::JobMode;

pub(super) fn receive_error(error: LiveError) -> quic::Error {
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

impl ControlledHost {
    /// Advertise one locally configured destination and opaque job ID. The
    /// controller must separately accept it with its own pinned source.
    pub fn offer_folder_upload(
        &mut self,
        request: ChannelRequest,
        configuration: Configuration,
        job_id: u64,
    ) -> Result<(), Error> {
        self.offer_folder_upload_mode(request, configuration, job_id, JobMode::Flat)
    }

    pub(in crate::session_startup) fn offer_folder_upload_mode(
        &mut self,
        request: ChannelRequest,
        mut configuration: Configuration,
        job_id: u64,
        mode: JobMode,
    ) -> Result<(), Error> {
        if !mode.upload_selected(&self.session.opened.selected) {
            return Err(Error::NotNegotiated);
        }
        if self.files.download.folder_job {
            return Err(Error::AlreadyAttached);
        }
        let job = ReceiverJob::for_host(job_id, configuration.directory.clone())
            .map_err(|error| Error::Receiver(receive_error(error)))?
            .with_mode(mode);
        // The exact tracked directory supplies conflict/publication evidence.
        configuration.directory = job.directory();
        let handle = u128::from(request.binding.parent.id);
        self.offer_file_target(
            request,
            handle,
            Target::Receive(configuration, Some(Box::new(job))),
        )
    }
    /// Original upload-job progress, retained through cancellation and cleanup.
    pub fn file_upload_sync_status(&self) -> Option<JobStatus> {
        self.files
            .upload
            .sync_receive
            .as_ref()
            .map(|job| job.status())
    }
}
