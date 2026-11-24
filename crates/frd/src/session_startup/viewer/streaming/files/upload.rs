//! One standing controller source, never a replayable ordinary file intake.
use super::{Absence, ControlledViewer, Error, SETUP_TIMEOUT, SendControl, SendPhase};
use fr_files::sync::SourceConfig;

pub(super) struct Lane {
    source: Option<SourceConfig>,
    control: SendControl,
    expecting: bool,
    cancelled: bool,
}
impl Lane {
    pub(super) fn new(source: SourceConfig, control: SendControl) -> Self {
        Self {
            source: Some(source),
            control,
            expecting: false,
            cancelled: false,
        }
    }
    pub(super) fn stop(&self) {
        self.control.cancel();
    }
    pub(super) fn without_control(&mut self) {
        self.source = None;
        self.control.end(Absence::Unavailable);
        self.control.cleaned(Some(Ok(())));
    }
    pub(super) fn viewer(&mut self, viewer: &mut ControlledViewer) {
        if !self.expecting {
            match viewer.expect_folder_upload_source(
                &mut self.source,
                self.control.clone(),
                SETUP_TIMEOUT,
            ) {
                Ok(()) => {
                    self.expecting = true;
                    self.control
                        .update(|status| status.phase = SendPhase::Negotiating);
                }
                Err(Error::Cancelled) if !viewer.is_closed() => {}
                Err(error) => {
                    self.expecting = true;
                    self.source = None;
                    self.control.end(Absence::Refused(error));
                    self.control.cleaned(Some(Ok(())));
                }
            }
            return;
        }
        self.observe(viewer);
        if viewer.file_send_negotiating() {
            return;
        }
        if self.control.cancel_requested()
            || viewer
                .file_upload_sync_status()
                .is_some_and(|job| job.failure.is_some())
        {
            if !self.cancelled {
                self.cancelled = true;
                if let Err(error) = viewer.cancel_file_send() {
                    self.control.end(Absence::Refused(error));
                }
            }
        } else if viewer.file_upload_sync_status().is_some()
            && let Err(error) = viewer.set_files_paused(self.control.pause_requested())
        {
            self.control.end(Absence::Refused(error));
            let _ = viewer.cancel_file_send();
            self.cancelled = true;
        }
        self.control.cleaned(viewer.try_finish_file_send_cleanup());
        self.observe(viewer);
    }
    pub(super) fn observe(&self, viewer: &ControlledViewer) {
        let report = viewer.file_batch_report();
        self.control.observe(
            report.as_ref(),
            viewer.file_stage(),
            viewer.file_progress(),
            viewer.file_is_paused(),
        );
        if let Some(job) = viewer.file_upload_sync_status() {
            self.control.record_folder_job(job);
        }
        if let Some(error) = viewer.file_send_failure() {
            self.control.end(Absence::SetupFailed(error));
        }
    }
}
