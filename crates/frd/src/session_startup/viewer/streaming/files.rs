//! `fr connect --control --send`: the user's explicit selection rides THIS
//! controller's one-use drop lane (`file-channel-scope`), never a new grant,
//! connection or retry.
//!
//! Ordering is the safety property: `viewer` runs at the start of every
//! network turn, before the controller drives, so the drop expectation exists
//! before this controller can answer its first control challenge. The host
//! offers only after that renewal (`running::streaming::files`), hence never
//! to a viewer that does not expect it. The batch starts only once the lane
//! is established; reading, hashing and sending stay on the sender's bounded
//! disk thread. Status is content-free (indices, sizes, typed outcomes).
use super::super::controlled::ControlledViewer;
use crate::native_files::{Absence, SETUP_TIMEOUT, SendControl, SendPhase, SendRequest};
use fr_files::{sender::Stage, session::Permission};

pub(in crate::session_startup) struct Lane {
    request: Option<SendRequest>,
    control: SendControl,
    expecting: bool,
}
impl Lane {
    pub(in crate::session_startup) fn new(request: SendRequest, control: SendControl) -> Self {
        Self {
            request: Some(request),
            control,
            expecting: false,
        }
    }
    /// Bounded and nonblocking: no source is opened or read here.
    pub(in crate::session_startup) fn viewer(&mut self, viewer: &mut ControlledViewer) {
        if !self.expecting {
            let Some(request) = &self.request else {
                return;
            };
            // The user's explicit `--send` is this side's local permission.
            match viewer.expect_file_drop(Permission::new(true), request.policy, SETUP_TIMEOUT) {
                Ok(()) => {
                    self.expecting = true;
                    self.control
                        .update(|status| status.phase = SendPhase::Negotiating);
                }
                // Only a momentarily stale view refuses a live controller here
                // (fresh permission, selection already checked). Nothing was
                // spent: no channel, no deadline. Retry next turn; a host offer
                // that races ahead of it fails this session, as any unexpected
                // attachment record does, and never publishes anything.
                Err(fr_files::sender::Error::Cancelled) if !viewer.is_closed() => {}
                Err(error) => {
                    self.expecting = true;
                    self.request = None;
                    self.control.end(Absence::Refused(error));
                }
            }
            return;
        }
        if self.request.is_some() && !viewer.file_send_negotiating() {
            let Some(request) = self.request.take() else {
                return;
            };
            if viewer.file_stage() == Some(Stage::Idle) {
                match viewer.send_files(request.selection, request.lifetime) {
                    Ok(()) => self
                        .control
                        .update(|status| status.phase = SendPhase::Sending),
                    Err(error) => self.control.end(Absence::Refused(error)),
                }
            } else {
                let failure = viewer
                    .file_failure()
                    .unwrap_or(fr_files::sender::Error::Closed);
                self.control.end(Absence::SetupFailed(failure));
            }
        }
        self.observe(viewer);
    }
    /// Copy the ordered receipts, including after closure or source reaping.
    pub(in crate::session_startup) fn observe(&self, viewer: &ControlledViewer) {
        if let Some(report) = viewer.file_batch_report() {
            self.control.update(|status| {
                status.report = Some(report);
                if report.complete {
                    status.phase = SendPhase::Finished;
                }
            });
        }
    }
}
