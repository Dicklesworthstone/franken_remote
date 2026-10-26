//! Client-selected destination registration, cancellation and retained status.
//! The controlling runtime owns the actual receiver and records each disk result.
use super::ControlledViewer;
use crate::native_files::{Absence, ReceiveControl, ReceivePhase, ReceiveRequest, SETUP_TIMEOUT};
use fr_files::{quic::Configuration, sender::Error};

pub(in crate::session_startup) struct Lane {
    request: Option<ReceiveRequest>,
    control: ReceiveControl,
    expecting: bool,
    cancelled: bool,
}
impl Lane {
    pub(super) fn new(request: ReceiveRequest, control: ReceiveControl) -> Self {
        Self {
            request: Some(request),
            control,
            expecting: false,
            cancelled: false,
        }
    }
    pub(super) fn viewer(&mut self, viewer: &mut ControlledViewer) {
        if !self.expecting {
            let Some(request) = &self.request else {
                return;
            };
            // A momentary presentation gap can refuse an unstarted expectation.
            // Retain the same pinned directory and permission for that retry.
            let configuration = Configuration {
                directory: request.configuration.directory.clone(),
                permission: request.configuration.permission.clone(),
                policy: request.configuration.policy,
                reply_lifetime: request.configuration.reply_lifetime,
            };
            match viewer.expect_file_download_with_control(
                configuration,
                self.control.clone(),
                SETUP_TIMEOUT,
            ) {
                Ok(()) => {
                    self.request = None;
                    self.expecting = true;
                    self.control.set_phase(ReceivePhase::Negotiating);
                }
                Err(Error::Cancelled) if !viewer.is_closed() => {}
                Err(error) => {
                    self.request = None;
                    self.expecting = true;
                    self.control.end(Absence::SetupFailed(error));
                    self.control.cleaned(Ok(()));
                }
            }
            return;
        }
        if viewer.file_receive_negotiating() {
            return;
        }
        self.observe(viewer);
        if self.control.cancel_requested() && !self.cancelled {
            self.cancelled = true;
            if let Err(error) = viewer.cancel_file_receive() {
                self.control.end(Absence::Refused(error));
            }
        }
        if let Some(error) = viewer.file_receive_failure() {
            self.control.end(Absence::ReceiveFailed(error));
        } else if viewer.file_receive_state().is_none()
            && let Some(error) = viewer.file_receive_setup_failure()
        {
            self.control.end(Absence::SetupFailed(error));
        }
        let _ = viewer.try_finish_file_receive_cleanup();
        self.observe(viewer);
    }
    pub(super) fn observe(&self, viewer: &ControlledViewer) {
        self.control.progress(viewer.file_receive_progress());
        if let Some(receipt) = viewer.file_receive_result() {
            self.control.observe(receipt);
        }
    }
    pub(super) fn without_control(&mut self) {
        self.request = None;
        self.control.end(Absence::Unavailable);
        self.control.cleaned(Ok(()));
    }
}
