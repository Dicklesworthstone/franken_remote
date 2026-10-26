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

mod download;

/// At most one explicitly selected source and destination on this controller.
/// Requests and status handles are independent; neither creates the other.
#[derive(Default)]
pub(in crate::session_startup) struct Lane {
    send: Option<Box<Sending>>,
    receive: Option<Box<download::Lane>>,
}
impl Lane {
    pub(in crate::session_startup) fn has_send(&self) -> bool {
        self.send.is_some()
    }
    pub(in crate::session_startup) fn has_receive(&self) -> bool {
        self.receive.is_some()
    }
    pub(in crate::session_startup) fn add_send(
        &mut self,
        request: SendRequest,
        control: SendControl,
    ) -> Result<(), Absence> {
        if self.send.is_some() {
            return Err(Absence::Unavailable);
        }
        self.send = Some(Box::new(Sending::new(request, control)));
        Ok(())
    }
    pub(in crate::session_startup) fn add_receive(
        &mut self,
        request: crate::native_files::ReceiveRequest,
        control: crate::native_files::ReceiveControl,
    ) -> Result<(), Absence> {
        if self.receive.is_some() {
            return Err(Absence::Unavailable);
        }
        self.receive = Some(Box::new(download::Lane::new(request, control)));
        Ok(())
    }
    pub(in crate::session_startup) fn viewer(&mut self, viewer: &mut ControlledViewer) {
        if let Some(lane) = &mut self.send {
            lane.viewer(viewer);
        }
        if let Some(lane) = &mut self.receive {
            lane.viewer(viewer);
        }
    }
    pub(in crate::session_startup) fn observe(&self, viewer: &ControlledViewer) {
        if let Some(lane) = &self.send {
            lane.observe(viewer);
        }
        if let Some(lane) = &self.receive {
            lane.observe(viewer);
        }
    }
    pub(in crate::session_startup) fn without_control(&mut self) {
        if let Some(lane) = &mut self.send {
            lane.request = None;
            lane.control.end(Absence::Unavailable);
            lane.control.cleaned(Some(Ok(())));
        }
        if let Some(lane) = &mut self.receive {
            lane.without_control();
        }
    }
}

pub(in crate::session_startup) struct Sending {
    request: Option<SendRequest>,
    policy: Policy,
    control: SendControl,
    expecting: bool,
    cancelled: bool,
}
impl Sending {
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
        // begin_batch only queues descriptors. Apply the latest pacing intent
        // BEFORE the next ordinary controller turn can start its first source.
        // Repeated observation never starts another batch or renews its deadline.
        if self.control.status().phase == SendPhase::Sending
            && let Err(error) = viewer.set_paused(self.control.pause_requested())
            && !matches!(viewer.stage(), Some(Stage::Finished | Stage::Closed))
        {
            // Refusal cannot become an implicit retry. Stop this completed file
            // attachment once; the original desktop context remains independent.
            self.control.end(Absence::Refused(error));
            if !self.cancelled {
                self.cancelled = true;
                if let Err(error) = viewer.cancel() {
                    self.control.end(Absence::Refused(error));
                }
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
        self.control.observe(
            report.as_ref(),
            viewer.stage(),
            viewer.progress(),
            viewer.paused(),
        );
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
    fn set_paused(&mut self, paused: bool) -> Result<(), Error>;
    fn paused(&self) -> Option<bool>;
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
        self.file_send_failure()
    }
    fn start(&mut self, request: SendRequest) -> Result<(), Error> {
        self.send_files(request.selection, request.lifetime)
    }
    fn cancel(&mut self) -> Result<(), Error> {
        self.cancel_file_send()
    }
    fn cleanup(&mut self) -> Option<Result<(), Error>> {
        self.try_finish_file_send_cleanup()
    }
    fn report(&self) -> Option<Report> {
        self.file_batch_report()
    }
    fn progress(&self) -> Option<Progress> {
        self.file_progress()
    }
    fn set_paused(&mut self, paused: bool) -> Result<(), Error> {
        self.set_files_paused(paused)
    }
    fn paused(&self) -> Option<bool> {
        self.file_is_paused()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod pause_tests;
