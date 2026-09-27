//! Terminal reports are consumed before any subsequent application dispatch.
use super::Error;
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::{self, Route, StreamRoute};
use fr_wire::{
    Kind,
    authority::Binding,
    closure::{self, Closed},
    input::{InputDelivery, InputDirection},
};

pub(super) fn receive(
    retained: &mut Option<Closed>,
    route: Route,
    bytes: &[u8],
    expected: StreamRoute,
    binding: Binding,
    limits: &ProtocolLimits,
) -> Option<Error> {
    if bytes.get(6..8) != Some(&(Kind::Closed as u16).to_be_bytes()) {
        return None;
    }
    if route != Route::Stream(expected) {
        return Some(Error::Transport(quic::Error::WrongRoute));
    }
    Some(
        match closure::decode_closed(
            bytes,
            binding,
            limits,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        ) {
            Ok(report) => {
                // Stop this receive batch by returning the existing dispatch error
                // path. SessionDrive/controlled ownership fences input, stops renewal,
                // and cancels local work. No later record or action can be dispatched
                // from this batch, and neither counters nor timestamps are fabricated.
                *retained = Some(report);
                Error::RemoteClosed(report)
            }
            Err(_) => Error::Transport(quic::Error::Malformed),
        },
    )
}

impl super::ViewerSession {
    /// End an observation-only session and request its original host's final
    /// report. Ordinary I/O and renewal stop at CALL time, before polling this
    /// future. No normal application dispatch runs during the closing exchange.
    ///
    /// Callers owning media must first stop presentation and other native work;
    /// this control-session API does not claim to reap another owner's decoder.
    /// Control-intent sessions refuse and close locally, retaining their existing
    /// input-cleanup obligations instead of bypassing lease-specific teardown.
    ///
    /// Waiting uses the original session context (still externally cancellable)
    /// and destination guard. It never resets cancellation or renews authority.
    /// The existing 250-ms exchange budget is further capped by the ORIGINAL
    /// silence and pending-response deadlines. Drop, refusal, timeout and success
    /// all finish local teardown. There is no reconnect or retry on this owner.
    ///
    /// The outcome distinguishes request ACK, the exact optional host report and
    /// transport failure. A received report survives even a failed final ACK and
    /// remains accessible via `closed_report`; absent reports never become zero
    /// effects or successful cleanup. Existing per-action receipts are unchanged.
    pub fn disconnect(
        &mut self,
        reason: closure::Reason,
    ) -> impl std::future::Future<Output = Result<quic::CloseOutcome, Error>> + '_ {
        // Install before any preflight or transport callback: an unwinding
        // preparation must fence this same session, not only an awaited future.
        let ending = Disconnect(self);
        let viewer = &mut *ending.0;
        let prepared = viewer.begin_disconnect(reason);
        async move {
            let outcome = prepared.await?;
            if let Some(report) = outcome.report {
                ending.0.remote_closed = Some(report);
            }
            drop(ending);
            Ok(outcome)
        }
    }
    /// Separate owned transport preparation so the streaming owner can fence its
    /// receiver/decoder and still use this EXACT control-session closing path.
    /// Its caller must install a teardown guard before invoking this method.
    pub(super) fn begin_disconnect(
        &mut self,
        reason: closure::Reason,
    ) -> impl std::future::Future<Output = Result<quic::CloseOutcome, Error>> + use<> {
        let ready = self.check().and_then(|()| {
            if self.opened.selection.role != fr_wire::negotiation::Role::Observe {
                return Err(Error::Order);
            }
            Ok(self
                .responder
                .response_deadline()
                .map_or(self.heard_until, |at| at.0.min(self.heard_until)))
        });
        // Fence ordinary session methods without cancelling the context that
        // still owns the one terminal exchange. The external stop handle and
        // immutable transport guard continue to cancel that exchange normally.
        self.closed = true;
        self.responder.stop();
        if let Some(clock) = &mut self.clock {
            clock.stop();
        }
        let prepared = ready.map(|until| {
            Box::pin(self.transport.close_with_request(
                &self.cx,
                &self.connection,
                self.routes,
                Binding {
                    channel: self.opened.binding.id,
                    session: self.opened.binding.remote_session,
                },
                closure::CloseRequest { reason },
                until,
            ))
        });
        if prepared.is_err() {
            self.close();
        }
        async move { Ok(prepared?.await) }
    }
}
struct Disconnect<'a>(&'a mut super::ViewerSession);
impl Drop for Disconnect<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}
