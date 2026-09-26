//! Persistent control, input and observation service on one admitted connection.
//! The native Driver remains independently polled; no native operation runs here.
use super::{Error, HostSession, ObservationControl, Services};
mod clipboard;
pub(crate) mod files;
pub(super) mod revocation;
use crate::{
    input_agent::{InputReply, Reply, Status},
    input_quic::{self, Progress, QuicInput, Routes, control::ControlRenewal},
    input_watchdog::{Control, StopReason},
};
use fr_core::{ids::InputTicketId, time::HostInstant};
use fr_transport::quic::{ControlRoutes, Disposition, Messages, QuicRecords, Route};
use fr_wire::{Kind, negotiation::Role};
use std::{future::Future, time::Duration};

/// Consumes the existing initialized input owner and running host session. It
/// never grants control or constructs another native executor. The enclosing
/// application still supplies fresh presentation/target evidence and drives the
/// native watchdog; media work stays outside this task.
pub struct ControlledHost {
    pub(super) session: HostSession,
    input: QuicInput,
    renewal: ControlRenewal,
    ticket_turn: bool,
    submitted: super::input_wake::Submitted,
    clipboard: Option<crate::clipboard_quic::Bridge>,
    clipboard_setup: crate::session_startup::clipboard::Setup,
    files: files::Slot,
    terminal_report: Option<Result<(), fr_transport::quic::Error>>,
    terminal_registered: bool,
}
impl std::fmt::Debug for ControlledHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlledHost")
            .field("native", &self.input.status())
            .finish_non_exhaustive()
    }
}
impl HostSession {
    /// Move the broker's initialized owner (or an already authorized negotiated
    /// input owner) into steady state. Equal session IDs on a different native
    /// authority or QUIC connection do not suffice. Failure drops/fences both
    /// consumed owners; it never makes the OS seat available before cleanup.
    pub fn into_controlled(mut self, mut input: QuicInput) -> Result<ControlledHost, Error> {
        self.check()?;
        self.opened
            .peer
            .check(&self.opened.cx, Role::RequestControl)?;
        if self.opened.selected.role != Role::RequestControl
            || input.routes().results().messages != Messages::InputFeedback
            || input.protocol_limits() != self.opened.selected.limits
        {
            return Err(Error::Order);
        }
        let renewal = input
            .control_renewal(
                self.opened.control.clone(),
                &self.opened.transport,
                self.opened.routes,
            )
            .map_err(Error::ControlRenewal)?;
        let reporting = self.revocation_reporting.take();
        let mut host = ControlledHost {
            session: self,
            input,
            renewal,
            ticket_turn: false,
            submitted: super::input_wake::Submitted::default(),
            clipboard: None,
            clipboard_setup: crate::session_startup::clipboard::Setup::default(),
            files: files::Slot::default(),
            terminal_report: None,
            terminal_registered: false,
        };
        if let Some(reporting) = reporting {
            host.arm_revocation_reporting(reporting)?;
        }
        Ok(host)
    }
}
impl ControlledHost {
    pub fn control(&self) -> Control {
        self.input.control()
    }
    pub fn native_status(&self) -> Status {
        self.input.status()
    }
    pub fn last_reply(&self) -> Option<InputReply> {
        self.input.last_reply()
    }
    pub fn last_reconciliation(&self) -> Option<Reply> {
        self.input.last_reconciliation()
    }
    pub fn control_renewed_until(&self) -> Option<HostInstant> {
        self.renewal.renewed_until()
    }
    pub fn observation_renewed_until(&self) -> Option<HostInstant> {
        self.session.renewed_until()
    }
    /// Loan the same checked connection to its bounded media/clock owners.
    /// Replacing the connection is not supported and is fenced on the next turn.
    pub fn io(&mut self) -> Result<(&mut QuicRecords, ControlRoutes), Error> {
        if self.input.control().is_stopped() {
            self.close();
            return Err(Error::Closed);
        }
        self.session.io()
    }
    /// Fence input before ending observation or cancelling this viewer's region.
    /// Native cleanup and destruction progress through the original Driver, not
    /// through this call. Other viewers' share-session capture is not cancelled.
    pub fn close(&mut self) {
        self.input.control().stop(StopReason::LocalRevoke);
        self.files.stop();
        if let Some(clipboard) = &self.clipboard {
            clipboard.stop();
        }
        self.clipboard_setup.stop();
        self.renewal.stop();
        self.session.close();
    }
    /// Collect the actual terminal native result after connection closure. This
    /// neither transmits bytes nor replays an effect. Repeat while an entered OS
    /// call is still completing; `None` is not a fabricated zero-effect receipt.
    pub fn collect_after_close(&mut self) -> Result<Option<InputReply>, input_quic::Error> {
        if !self.session.opened.transport.is_closed() {
            return Err(input_quic::Error::InvalidRoutes);
        }
        match self
            .input
            .service(&mut self.session.opened.transport, || false)
        {
            Err(input_quic::Error::Closed) | Ok(_) => Ok(self.input.last_reply()),
            Err(error) => Err(error),
        }
    }
    pub(super) async fn drive_services(
        &mut self,
        wait: Duration,
        fresh_nonce: &mut impl FnMut() -> Result<u128, ()>,
        fresh_ticket: &mut impl FnMut() -> Option<InputTicketId>,
        other: &mut impl Services,
    ) -> Result<(), Error> {
        // A synchronous watchdog/local fence must win over normal transport
        // checks, which would otherwise drop the socket before reporting it.
        if self.input.control().is_stopped() {
            if self.terminal_registered {
                self.close();
                return Err(Error::Closed);
            }
            let reason = self
                .input
                .control()
                .reason()
                .unwrap_or(StopReason::AuthorityEnded);
            // Delivery is separately retained by revocation_delivery. Whether
            // acknowledged or lost, this service's outcome is terminal closure.
            // Boxed: its state would otherwise inflate every drive future.
            let _ = Box::pin(self.revoke_and_close(reason)).await;
            return Err(Error::Closed);
        }
        let mut services = InputServices {
            input: &mut self.input,
            files: &mut self.files,
            clipboard: &mut self.clipboard,
            clipboard_setup: &mut self.clipboard_setup,
            cx: self.session.opened.cx.clone(),
            parent: self.session.opened.binding,
            renewal: &mut self.renewal,
            observation: self.session.opened.control.clone(),
            control: self.session.opened.routes,
            ticket_turn: &mut self.ticket_turn,
            submitted: &mut self.submitted,
            ticket: fresh_ticket,
            other,
        };
        self.session
            .drive_inner(wait, fresh_nonce, &mut services)
            .await
    }
    /// Service all authorities, native receipts and input tickets during idle,
    /// traffic, and a pending installed-Tailscale refresh. The two supplied ID
    /// sources must be host-owned, unpredictable and non-reusing. They are not
    /// called when their bounded owners cannot issue a new challenge/ticket.
    ///
    /// Input records go only to the canonical owner. `other` receives unrelated
    /// application records and must do bounded nonblocking work. A Blocked reply
    /// preserves transport ownership. Even abandoning an UNPOLLED drive revokes
    /// input immediately; the same independently polled Driver performs cleanup.
    pub fn drive<'a>(
        &'a mut self,
        wait: Duration,
        mut fresh_nonce: impl FnMut() -> Result<u128, ()> + 'a,
        mut fresh_ticket: impl FnMut() -> Option<InputTicketId> + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let operation = Operation {
            host: self,
            complete: false,
        };
        async move {
            let mut operation = operation;
            operation
                .host
                .drive_services(wait, &mut fresh_nonce, &mut fresh_ticket, &mut other)
                .await?;
            operation.complete = true;
            Ok(())
        }
    }
}
impl Drop for ControlledHost {
    fn drop(&mut self) {
        self.close();
    }
}
struct Operation<'a> {
    host: &'a mut ControlledHost,
    complete: bool,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.host.close();
        }
    }
}
struct InputServices<'a, T, F> {
    input: &'a mut QuicInput,
    files: &'a mut files::Slot,
    clipboard: &'a mut Option<crate::clipboard_quic::Bridge>,
    clipboard_setup: &'a mut crate::session_startup::clipboard::Setup,
    cx: asupersync::cx::Cx,
    parent: fr_wire::negotiation::ControlBinding,
    renewal: &'a mut ControlRenewal,
    observation: ObservationControl,
    control: ControlRoutes,
    ticket_turn: &'a mut bool,
    submitted: &'a mut super::input_wake::Submitted,
    ticket: &'a mut T,
    other: &'a mut F,
}
fn is_input(routes: Routes, route: Route) -> bool {
    route == Route::Stream(routes.actions())
        || routes
            .pointer()
            .is_some_and(|p| route == Route::Datagram(p))
}
impl<T, F> Services for InputServices<'_, T, F>
where
    T: FnMut() -> Option<InputTicketId>,
    F: Services,
{
    fn permitted(&mut self) -> bool {
        self.files.permitted()
            && self.clipboard_setup.permits_io()
            && self.renewal.permitted()
            && self.other.permitted()
            && self
                .clipboard
                .as_ref()
                .is_none_or(crate::clipboard_quic::Bridge::permits_io)
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
    ) -> Result<(), Error> {
        // Local lifecycle permission must be refreshed before draining queued
        // input, including inside a long admission refresh. Native submission
        // still checks its own independent authority and platform state.
        if !self.other.permitted() {
            self.input.control().stop(StopReason::ViewInvalidated);
            return Err(Error::Closed);
        }
        if let Some((channel, granted)) = self
            .clipboard_setup
            .service(q, || {
                self.renewal.permitted() && self.observation.check().is_ok()
            })
            .map_err(Error::Clipboard)?
        {
            let result = clipboard::join(q, &self.cx, self.parent, self.input, channel, granted)
                .map(|(bridge, seed)| {
                    *self.clipboard = Some(bridge);
                    seed
                });
            self.clipboard_setup
                .joined(result)
                .map_err(Error::Clipboard)?;
        }
        if let Some(clipboard) = self.clipboard {
            clipboard
                .service(q, || self.observation.check().is_ok())
                .map_err(Error::Clipboard)?;
        }
        self.renewal
            .receive(q, |_, _| Ok(Disposition::Blocked))
            .map_err(Error::ControlRenewal)?;
        self.renewal
            .service(q, &mut *nonce)
            .map_err(Error::ControlRenewal)?;
        self.input
            .service(q, || self.observation.check().is_ok())
            .map_err(Error::Input)?;
        // Collection, not reverse-stream delivery, supplies the hint. Separate
        // sequence high-water marks reject both immediate and older replays;
        // ticket/authority traffic and zero-effect refusals never wake capture.
        if self.submitted.take(self.input.last_reply()) && self.renewal.permitted() {
            let at = self.observation.check().map_err(Error::Media)?.as_micros();
            self.other.input_submitted(at);
        }
        // A sustained ordered stream must not occupy every newly freed mailbox
        // slot forever. After a serviced input turn, give a due ticket ONE turn
        // before more input. An outstanding receipt still owns the slot first;
        // ordered actions retain priority over pointer traffic within receive.
        if *self.ticket_turn && self.input.can_accept_input() {
            self.input
                .renew_ticket(q, || self.observation.check().is_ok(), &mut *self.ticket)
                .map_err(Error::Input)?;
            *self.ticket_turn = false;
        }
        let received = self
            .input
            .receive(
                q,
                || self.observation.check().is_ok(),
                |_| false,
                |_, _| Ok(Disposition::Blocked),
            )
            .map_err(Error::Input)?;
        if received != 0 {
            *self.ticket_turn = true;
        } else {
            let progress = self
                .input
                .renew_ticket(q, || self.observation.check().is_ok(), &mut *self.ticket)
                .map_err(Error::Input)?;
            if progress == Progress::Stopped {
                return Err(Error::Closed);
            }
        }
        if !self.renewal.permitted() {
            return Err(Error::Closed);
        }
        // Bulk work gets one bounded receive/reply turn AFTER native input and
        // renewal. Its disk worker never blocks this authority/transport owner.
        self.files
            .service(q, || {
                self.renewal.permitted() && self.observation.check().is_ok()
            })
            .map_err(Error::Transport)?;
        self.other.maintain(q, nonce)
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        // These records belong to the owners serviced immediately before/after
        // observation dispatch, never to an application callback or a new queue.
        let kind = bytes.get(6..8);
        if self.files.owns(route, bytes)
            || self
                .clipboard
                .as_ref()
                .is_some_and(|c| c.owns_inbound(route))
            || self.clipboard_setup.owns(route, bytes)
            || is_input(self.input.routes(), route)
            || (route == Route::Stream(self.control.inbound)
                && (kind == Some(&(Kind::ChallengeResponse as u16).to_be_bytes())
                    || kind == Some(&(Kind::Challenge as u16).to_be_bytes())))
        {
            Ok(Disposition::Blocked)
        } else {
            self.other.receive(route, bytes)
        }
    }
}

#[cfg(test)]
pub(in crate::session_startup) mod tests;
