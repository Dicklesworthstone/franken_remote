//! The explicit selection rides THIS controller's one-use drop lane. The host
//! offers after the first control renewal, so even cancellation before control
//! must install the expectation and settle that original bounded handshake.
//! Source cancellation/cleanup never borrows or cancels the desktop context.
use super::super::controlled::ControlledViewer;
use crate::native_files::{
    Absence, SETUP_TIMEOUT, SendControl, SendIntake, SendPhase, SendRequest,
};
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
        self.install(Sending::new(request, control))
    }
    pub(in crate::session_startup) fn add_deferred_send(
        &mut self,
        control: SendControl,
    ) -> Result<(), Absence> {
        self.install(Sending::deferred(Policy::default(), control))
    }
    fn install(&mut self, sending: Sending) -> Result<(), Absence> {
        if self.send.is_some() {
            return Err(Absence::Unavailable);
        }
        self.send = Some(Box::new(sending));
        Ok(())
    }
    pub(in crate::session_startup) fn set_intake(&mut self, intake: SendIntake) {
        if let Some(send) = &mut self.send {
            send.intake = Some(intake);
        }
    }
    pub(in crate::session_startup) fn close_intake(&self) {
        if let Some(send) = &self.send {
            send.close_intake();
        }
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
            lane.close_intake();
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
    intake: Option<SendIntake>,
    original_deadline: Option<u64>,
    /// An explicit empty-source configuration, never an empty ATP batch.
    deferred: bool,
}
impl Sending {
    pub(in crate::session_startup) fn new(request: SendRequest, control: SendControl) -> Self {
        Self {
            policy: request.policy,
            request: Some(request),
            control,
            expecting: false,
            cancelled: false,
            intake: None,
            original_deadline: None,
            deferred: false,
        }
    }
    fn deferred(policy: Policy, control: SendControl) -> Self {
        control.update(|status| status.phase = SendPhase::WaitingForSelection);
        Self {
            request: None,
            policy,
            control,
            expecting: false,
            cancelled: false,
            intake: None,
            original_deadline: None,
            deferred: true,
        }
    }
    /// No source read, blocking join, alternate transport pump or renewed grant.
    pub(in crate::session_startup) fn viewer(&mut self, viewer: &mut impl Endpoint) {
        self.advance_intake(viewer);
        let cancel = self.control.cancel_requested();
        if cancel {
            self.close_intake();
            // Release queued descriptors now, but keep the agreed attachment's
            // policy/expectation. Skipping it would make the host's offer fatal.
            self.request = None;
        }
        if !self.expecting {
            match viewer.expect(self.policy) {
                Ok(()) => {
                    self.expecting = true;
                    if !self.deferred {
                        self.control
                            .update(|status| status.phase = SendPhase::Negotiating);
                    }
                }
                // Nothing was spent on a momentarily stale view: retry only
                // this unstarted expectation, not the source or a sent record.
                Err(Error::Cancelled) if !viewer.closed() => {}
                Err(error) => {
                    self.close_intake();
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
        if self.deferred && viewer.stage() != Some(Stage::Idle) {
            // The optional attachment still has a fixed setup deadline. No
            // selected source is needed to surface its refusal and retire the
            // original intake; a failed setup must not appear ready forever.
            self.close_intake();
            self.control.end(Absence::SetupFailed(
                viewer.failure().unwrap_or(Error::Closed),
            ));
            self.poll_cleanup(viewer);
            return;
        }
        if let Some(request) = self.request.take() {
            if viewer.stage() == Some(Stage::Idle) {
                let result = match self.original_deadline.take() {
                    Some(until) => viewer.start_before(request, until),
                    None => viewer.start(request),
                };
                match result {
                    Ok(()) => self
                        .control
                        .update(|status| status.phase = SendPhase::Sending),
                    Err(error) => {
                        self.close_intake();
                        self.control.end(Absence::Refused(error));
                        self.control.cleaned(Some(Ok(())));
                    }
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
    fn close_intake(&self) {
        if let Some(intake) = &self.intake {
            intake.close();
        }
    }
    fn advance_intake(&mut self, viewer: &mut impl Endpoint) {
        let Some(intake) = self.intake.clone() else {
            return;
        };
        intake.set_ready(false);
        if viewer.closed()
            || viewer.stage() == Some(Stage::Closed)
            || self.cancelled
            || self.control.cancel_requested()
            || self.control.status().phase == SendPhase::Ended
        {
            intake.close();
            return;
        }
        if !self.expecting || viewer.negotiating() || self.request.is_some() {
            return;
        }
        if let Some(report) = viewer.report() {
            self.observe(viewer);
            if !report.complete {
                return;
            }
            if report.stop.is_some() {
                intake.close();
                return;
            }
            // A complete report proves the original sources were joined. Keep
            // it in that batch's handle BEFORE clearing the sender's report.
            if viewer.collect_report() != Some(report) {
                return;
            }
        }
        if !viewer.can_start() {
            return;
        }
        if let Some(pending) = intake.take() {
            if self.deferred {
                // A concurrent setup cancellation and the first handoff have
                // one winner. The already-taken request must receive its own
                // terminal result if cancellation won; close cannot see it.
                if !self.control.accept_selection() {
                    pending.control.end(Absence::Refused(Error::Cancelled));
                    pending.control.cleaned(Some(Ok(())));
                    self.close_intake();
                    return;
                }
                // The setup handle never acquires authority over a later
                // selection, and no empty batch/publication is fabricated.
                self.deferred = false;
            }
            self.control = pending.control;
            self.request = Some(pending.request);
            self.original_deadline = Some(pending.until_us);
            self.cancelled = false;
        } else {
            intake.set_ready(true);
        }
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
impl Drop for Sending {
    fn drop(&mut self) {
        self.close_intake();
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
    fn start_before(&mut self, request: SendRequest, _until_us: u64) -> Result<(), Error> {
        self.start(request)
    }
    fn collect_report(&mut self) -> Option<Report> {
        None
    }
    fn can_start(&mut self) -> bool {
        !self.closed() && self.stage() == Some(Stage::Idle)
    }
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
    fn start_before(&mut self, request: SendRequest, until_us: u64) -> Result<(), Error> {
        self.send_files_before(request.selection, until_us)
    }
    fn collect_report(&mut self) -> Option<Report> {
        self.take_file_batch_report()
    }
    fn can_start(&mut self) -> bool {
        self.can_select_files()
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
