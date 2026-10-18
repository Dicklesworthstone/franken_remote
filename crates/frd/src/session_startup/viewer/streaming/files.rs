//! The explicit selection rides THIS controller's one-use drop lane. The host
//! offers after the first control renewal, so even cancellation before control
//! must install the expectation and settle that original bounded handshake.
//! Source cancellation/cleanup never borrows or cancels the desktop context.
use super::super::controlled::ControlledViewer;
use crate::native_files::{Absence, SETUP_TIMEOUT, SendControl, SendPhase, SendRequest};
use fr_files::{
    sender::{Error, Policy, Progress, Stage, batch::Report},
    session::Permission,
};

pub(in crate::session_startup) struct Lane {
    request: Option<SendRequest>,
    policy: Policy,
    control: SendControl,
    expecting: bool,
    cancelled: bool,
}
impl Lane {
    pub(in crate::session_startup) fn new(request: SendRequest, control: SendControl) -> Self {
        Self {
            policy: request.policy,
            request: Some(request),
            control,
            expecting: false,
            cancelled: false,
        }
    }
    /// No source read, blocking join, alternate transport pump or renewed grant.
    pub(in crate::session_startup) fn viewer(&mut self, viewer: &mut impl Endpoint) {
        let cancel = self.control.cancel_requested();
        if cancel {
            // Release queued descriptors now, but keep the agreed attachment's
            // policy/expectation. Skipping it would make the host's offer fatal.
            self.request = None;
        }
        if !self.expecting {
            match viewer.expect(self.policy) {
                Ok(()) => {
                    self.expecting = true;
                    self.control
                        .update(|status| status.phase = SendPhase::Negotiating);
                }
                // Nothing was spent on a momentarily stale view: retry only
                // this unstarted expectation, not the source or a sent record.
                Err(Error::Cancelled) if !viewer.closed() => {}
                Err(error) => {
                    self.expecting = true;
                    self.request = None;
                    self.control.end(Absence::Refused(error));
                }
            }
            return;
        }
        if viewer.negotiating() {
            self.observe(viewer);
            return;
        }
        if cancel {
            // Observe first: cancellation cannot rewrite a success that arrived
            // on the preceding network turn, nor invent rollback of an effect.
            self.observe(viewer);
            if self.control.status().report.is_some_and(|r| r.complete) {
                return;
            }
            if !self.cancelled {
                self.cancelled = true;
                if let Err(error) = viewer.cancel() {
                    self.control.end(Absence::Refused(error));
                }
            }
            self.poll_cleanup(viewer);
            if self.control.status().report.is_none()
                && self.control.snapshot().cleanup == Some(Ok(()))
            {
                self.control.end(Absence::Refused(Error::Cancelled));
            }
            return;
        }
        if let Some(request) = self.request.take() {
            if viewer.stage() == Some(Stage::Idle) {
                match viewer.start(request) {
                    Ok(()) => self
                        .control
                        .update(|status| status.phase = SendPhase::Sending),
                    Err(error) => self.control.end(Absence::Refused(error)),
                }
            } else {
                self.control.end(Absence::SetupFailed(
                    viewer.failure().unwrap_or(Error::Closed),
                ));
            }
        }
        // A failed sender also needs nonblocking cleanup while the desktop
        // stays open. Its stopped Slot no longer runs ordinary sender service.
        self.poll_cleanup(viewer);
    }
    fn poll_cleanup(&self, viewer: &mut impl Endpoint) {
        self.control.cleaned(viewer.cleanup());
        self.observe(viewer);
    }
    /// Called on ordinary turns AND after final parent cleanup. Keep the real
    /// ordered receipts; bytes queued alone never advance the phase to Finished.
    pub(in crate::session_startup) fn observe(&self, viewer: &impl Endpoint) {
        let report = viewer.report();
        self.control
            .observe(report.as_ref(), viewer.stage(), viewer.progress());
    }
}

/// The production adapter retains all admission, authority, publication and
/// cleanup decisions in `ControlledViewer`. Tests script only these boundaries,
/// not a replacement transport or evidence of a live session.
pub(in crate::session_startup) trait Endpoint {
    fn expect(&mut self, policy: Policy) -> Result<(), Error>;
    fn closed(&self) -> bool;
    fn negotiating(&self) -> bool;
    fn stage(&self) -> Option<Stage>;
    fn failure(&self) -> Option<Error>;
    fn start(&mut self, request: SendRequest) -> Result<(), Error>;
    fn cancel(&mut self) -> Result<(), Error>;
    fn cleanup(&mut self) -> Option<Result<(), Error>>;
    fn report(&self) -> Option<Report>;
    fn progress(&self) -> Option<Progress>;
}
impl Endpoint for ControlledViewer {
    fn expect(&mut self, policy: Policy) -> Result<(), Error> {
        self.expect_file_drop(Permission::new(true), policy, SETUP_TIMEOUT)
    }
    fn closed(&self) -> bool {
        self.is_closed()
    }
    fn negotiating(&self) -> bool {
        self.file_send_negotiating()
    }
    fn stage(&self) -> Option<Stage> {
        self.file_stage()
    }
    fn failure(&self) -> Option<Error> {
        self.file_failure()
    }
    fn start(&mut self, request: SendRequest) -> Result<(), Error> {
        self.send_files(request.selection, request.lifetime)
    }
    fn cancel(&mut self) -> Result<(), Error> {
        self.cancel_files()
    }
    fn cleanup(&mut self) -> Option<Result<(), Error>> {
        self.try_finish_file_cleanup()
    }
    fn report(&self) -> Option<Report> {
        self.file_batch_report()
    }
    fn progress(&self) -> Option<Progress> {
        self.file_progress()
    }
}

#[cfg(test)]
mod tests;
