//! One explicit viewer control request on the original admitted connection.
//! Observation renewal and clock exchange continue while the host decides and
//! initializes its native owner. A grant is not mapping or visible-frame proof.
use super::super::{ViewerSession, now};
use crate::{input_quic::NegotiatedInput, media::clock::ClockSync};
use fr_client::{
    control_grant::RequestControl,
    input::{ClientInstant, InputClient, Policy},
};
use fr_transport::quic::{self, Disposition, Route};
use fr_wire::{
    Kind,
    control::{Granted, Request},
    negotiation::Role,
};
use std::{future::Future, time::Duration};

use super::Error;

impl ViewerSession {
    /// Send one explicit request using THIS session's completed input attachment
    /// and existing clock exchange. The original two-second request deadline
    /// includes unpolled time, observation service, native initialization and
    /// transport backpressure. No attempt is replayed or automatically renewed.
    ///
    /// The returned input owner carries the host's actual initial ticket, but
    /// cannot send actions until mapping and fresh presentation are confirmed.
    /// Join it to the actual decoder through `PresentedInput`, then transfer
    /// these same channel and clock owners into `into_controlled`.
    ///
    /// Unrelated media/configuration records stay in the transport's bounded
    /// queues. The caller must continue/revalidate its decoder after this stage;
    /// it cannot treat this exchange as fresh presentation or local approval.
    /// Any error or abandonment, including an UNPOLLED future, closes this
    /// session before dropping the request. A new user request needs a new session.
    pub fn request_control<'a>(
        &'a mut self,
        channels: &'a NegotiatedInput,
        clock: &'a mut ClockSync,
        request: Request,
        policy: Policy,
    ) -> impl Future<Output = Result<(Granted, InputClient), Error>> + 'a {
        // Construct outside the async body: scheduling cannot restart the TTL.
        let pending = now(&self.cx).map_err(Error::Session).and_then(|t| {
            RequestControl::new(
                request,
                channels.channel_binding(),
                self.opened.selection.limits,
                ClientInstant(t),
            )
            .map_err(Error::ControlRequest)
        });
        let guard = Attempt {
            session: self,
            clock,
            complete: false,
        };
        async move {
            let mut guard = guard;
            let mut pending = pending?;
            guard.check(channels, request, pending.deadline().0)?;
            let result = guard
                .exchange(channels, request, &mut pending, policy)
                .await?;
            guard.complete = true;
            Ok(result)
        }
    }
}
struct Attempt<'a> {
    session: &'a mut ViewerSession,
    clock: &'a mut ClockSync,
    complete: bool,
}
impl Attempt<'_> {
    fn check(
        &mut self,
        channels: &NegotiatedInput,
        request: Request,
        until: u64,
    ) -> Result<u64, Error> {
        self.session.check().map_err(Error::Session)?;
        let selected = &self.session.opened.selection;
        if selected.role != Role::RequestControl
            || !selected
                .capabilities
                .iter()
                .any(|c| c.name == crate::input_quic::grant::CAPABILITY && c.version == 1)
        {
            return Err(Error::ControlNotNegotiated);
        }
        if request.parent != self.session.opened.binding
            || channels.limits() != selected.limits
            || !self.clock.matches_viewer(
                &self.session.transport,
                self.session.opened.binding,
                selected.limits,
            )
        {
            return Err(Error::WrongBinding);
        }
        channels
            .check_request(&self.session.transport, request)
            .map_err(Error::Input)?;
        channels
            .viewer_routes(&self.session.transport)
            .map_err(Error::Input)?;
        let t = now(&self.session.cx).map_err(Error::Session)?;
        if t >= until {
            return Err(Error::Expired);
        }
        Ok(t)
    }
    async fn exchange(
        &mut self,
        channels: &NegotiatedInput,
        request: Request,
        pending: &mut RequestControl,
        policy: Policy,
    ) -> Result<(Granted, InputClient), Error> {
        let until = pending.deadline().0;
        loop {
            self.check(channels, request, until)?;
            self.clock
                .receive(&mut self.session.transport, |_, _| Ok(Disposition::Blocked))
                .map_err(Error::Clock)?;
            self.clock
                .service(&mut self.session.transport)
                .map_err(Error::Clock)?;
            let sample = self
                .clock
                .correlation(&mut self.session.transport)
                .map_err(Error::Clock)?;
            let t = self.check(channels, request, until)?;
            if sample.is_some()
                && let Some(bytes) = pending
                    .pending(ClientInstant(t))
                    .map_err(Error::ControlRequest)?
            {
                let cx = &self.session.cx;
                match self.session.transport.send(
                    cx,
                    Route::Stream(self.session.routes.outbound),
                    bytes,
                    until,
                    || now(cx).is_ok_and(|n| n < until),
                ) {
                    Ok(()) => pending
                        .sent(ClientInstant(now(cx).map_err(Error::Session)?))
                        .map_err(Error::ControlRequest)?,
                    Err(quic::Error::Backpressure) => {}
                    Err(error) => return Err(Error::Session(error.into())),
                }
            }
            let inbound = Route::Stream(self.session.routes.inbound);
            let cx = self.session.cx.clone();
            let mut reply = None;
            let mut failure = None;
            let wait = Duration::from_micros((until - t).min(5_000));
            let driven = self
                .session
                .drive(wait, |route, bytes| {
                    if route != inbound
                        || bytes.get(6..8) != Some(&(Kind::LeaseGranted as u16).to_be_bytes())
                        || reply.is_some()
                    {
                        return Ok(Disposition::Blocked);
                    }
                    let result = (|| {
                        let sample = sample.ok_or(Error::WrongBinding)?;
                        let at = ClientInstant(now(&cx).map_err(Error::Session)?);
                        pending
                            .accept(bytes, sample, policy, at)
                            .map_err(Error::ControlRequest)
                    })();
                    match result {
                        Ok(value) => {
                            reply = Some(value);
                            Ok(Disposition::Consumed)
                        }
                        Err(error) => {
                            failure = Some(error);
                            Err(())
                        }
                    }
                })
                .await;
            if let Some(error) = failure {
                return Err(error);
            }
            driven.map_err(Error::Session)?;
            let at = self.check(channels, request, until)?;
            if let Some((grant, mut input)) = reply {
                input
                    .tick(ClientInstant(at))
                    .map_err(|e| Error::View(fr_client::input::presentation::Error::Input(e)))?;
                if input.ticket_deadline().is_none_or(|d| at >= d.0) {
                    return Err(Error::Expired);
                }
                return Ok((grant, input));
            }
        }
    }
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.session.close();
            self.clock.stop();
        }
    }
}

#[cfg(test)]
mod tests;
