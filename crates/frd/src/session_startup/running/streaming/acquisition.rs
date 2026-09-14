//! Live grant acquisition on the original capture/session service. Native input
//! initialization and cleanup stay with the independently polled Driver.
use super::{Error, Fence, Guarded, Host, Operation, Services, StreamingHost};
use crate::{
    input_agent::{Driver, Seat, Status},
    input_quic::{
        NegotiatedInput,
        grant::{Error as GrantError, Event, GrantBroker},
    },
    input_watchdog::{Control, StopReason},
};
use fr_core::{
    ids::{InputLeaseId, InputTicketId},
    input_submission::{InputSink, PlatformError},
};
use fr_transport::quic::{Disposition, QuicRecords, Route};
use fr_wire::control::{Request, Target};
use std::{future::Future, time::Duration};

/// A local callback, never a wire callback. Return the CURRENT qualified target
/// every turn. Approval is explicit; merely returning a target never approves.
/// Callbacks must not block on UI or OS work. A returned Driver must immediately
/// enter the application's independent authority task, not the capture future.
pub enum LocalControl<'a> {
    Pending(PendingControl<'a>),
    Active { request: Request, control: Control },
}
/// Borrowed local approval capability. Neither connection nor broker escapes;
/// the request is copied metadata, not evidence of consent or media readiness.
pub struct PendingControl<'a> {
    broker: &'a mut GrantBroker,
    connection: &'a QuicRecords,
}
impl PendingControl<'_> {
    pub fn request(&self) -> Option<Request> {
        self.broker.request()
    }
    /// True only while the host's current view evidence remains valid.
    /// Approval is still explicit and rechecks readiness at publication.
    pub fn view_ready(&mut self) -> Result<bool, GrantError> {
        self.broker.view_ready(self.connection)
    }
    pub fn native_status(&self) -> Option<Status> {
        self.broker.native_status()
    }
    pub fn deny(&mut self) {
        self.broker.deny();
    }
    pub fn stop(&mut self) {
        self.broker.stop();
    }
    /// The existing broker checks current admission/readiness and reserves the
    /// shared Seat before minting credentials. Native factory/cleanup run off the
    /// authority path. Busy-seat refusal leaves the request available to the UI.
    pub fn approve<S, F, C>(
        &mut self,
        target: Target,
        fresh: impl FnOnce() -> Option<(InputLeaseId, InputTicketId)>,
        factory: F,
        cleanup: C,
    ) -> Result<Driver, GrantError>
    where
        S: InputSink + 'static,
        F: FnOnce() -> Result<S, PlatformError> + Send + 'static,
        C: FnMut(&mut S) -> bool + Send + 'static,
    {
        self.broker
            .approve(self.connection, target, fresh, factory, cleanup)
    }
}

pub(super) struct Acquisition {
    broker: Option<GrantBroker>,
    request: Option<Request>,
    active: Option<Control>,
    current: Option<Target>,
    ready: bool,
    failure: Option<GrantError>,
}
impl Acquisition {
    fn new(host: &mut StreamingHost, seat: Seat, channels: NegotiatedInput) -> Result<Self, Error> {
        if host.stream.served || !matches!(host.host, Host::Observe(_)) {
            return Err(Error::Order);
        }
        // The existing attachment proof already binds the connection, view and
        // display. Also require this capture pipeline's exact configuration.
        let session = host.host.session()?;
        let (_, configuration) = channels
            .host_scope(&session.opened.transport)
            .map_err(Error::Input)?;
        if configuration
            != host
                .stream
                .sender
                .feedback_view()
                .map_err(Error::MediaTransport)?
        {
            return Err(Error::Order);
        }
        let broker = session
            .negotiated_control_broker(seat, channels)
            .map_err(Error::ControlGrant)?;
        Ok(Self {
            broker: Some(broker),
            request: None,
            active: None,
            current: None,
            ready: false,
            failure: None,
        })
    }
    pub(super) async fn drive(
        &mut self,
        host: &mut Host,
        wait: Duration,
        nonce: &mut impl FnMut() -> Result<u128, ()>,
        ticket: &mut impl FnMut() -> Option<InputTicketId>,
        other: &mut impl Services,
        local: &mut impl FnMut(LocalControl<'_>) -> Result<Option<Target>, GrantError>,
    ) -> Result<(), Error> {
        let result = host
            .drive(
                wait,
                nonce,
                ticket,
                &mut GrantServices {
                    acquisition: self,
                    local,
                    other,
                },
            )
            .await;
        if let Some(error) = self.failure.take() {
            return Err(Error::ControlGrant(error));
        }
        result?;
        if self.ready && self.broker.is_some() {
            let Host::Observe(mut session) = std::mem::replace(host, Host::Closed) else {
                return Err(Error::Order);
            };
            session.check()?;
            let input = self
                .broker
                .take()
                .ok_or(Error::Order)?
                .finish(&session.opened.transport, self.current)
                .map_err(Error::ControlGrant)?;
            let controlled = session.into_controlled(input)?;
            self.active = Some(controlled.control());
            *host = Host::Control(controlled);
        }
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(control) = &self.active {
            control.stop(StopReason::LocalRevoke);
        }
        if let Some(broker) = &mut self.broker {
            broker.stop();
        }
    }
}
impl Drop for Acquisition {
    fn drop(&mut self) {
        self.stop();
    }
}
struct GrantServices<'a, L, S> {
    acquisition: &'a mut Acquisition,
    local: &'a mut L,
    other: &'a mut S,
}
impl<L, S> Services for GrantServices<'_, L, S>
where
    L: FnMut(LocalControl<'_>) -> Result<Option<Target>, GrantError>,
    S: Services,
{
    fn input_submitted(&mut self, at: u64) {
        self.other.input_submitted(at);
    }
    fn permitted(&mut self) -> bool {
        if let Some(control) = self.acquisition.active.clone() {
            let a = &mut *self.acquisition;
            let checked = a.request.ok_or(GrantError::NoRequest).and_then(|request| {
                a.current = (self.local)(LocalControl::Active {
                    request,
                    control: control.clone(),
                })?;
                if a.current == Some(request.target) {
                    Ok(())
                } else {
                    Err(GrantError::TargetChanged)
                }
            });
            if let Err(error) = checked {
                a.failure.get_or_insert(error);
                control.stop(StopReason::ViewInvalidated);
                return false;
            }
        }
        self.acquisition.failure.is_none()
            && self
                .acquisition
                .active
                .as_ref()
                .is_none_or(|c| !c.is_stopped())
            && self.other.permitted()
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
    ) -> Result<(), Error> {
        let a = &mut *self.acquisition;
        let result = (|| {
            if let Some(broker) = &mut a.broker {
                // Approval consumes broker.request(); preserve only that exact
                // request, never reconstruct it from a later local target.
                if broker.native_status().is_none() {
                    a.request = broker.request();
                }
                a.current = (self.local)(LocalControl::Pending(PendingControl {
                    broker,
                    connection: q,
                }))?;
                a.ready = broker.service(q, a.current)? == Event::GrantQueued;
            } else {
                let request = a.request.ok_or(GrantError::NoRequest)?;
                let control = a.active.clone().ok_or(GrantError::NativeNotReady)?;
                a.current = (self.local)(LocalControl::Active {
                    request,
                    control: control.clone(),
                })?;
                if a.current != Some(request.target) {
                    control.stop(StopReason::ViewInvalidated);
                    return Err(GrantError::TargetChanged);
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            a.failure = Some(error);
            a.stop();
            return Err(Error::ControlGrant(error));
        }
        self.other.maintain(q, nonce)
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        let a = &mut *self.acquisition;
        // Once grant bytes are queued, retain any immediate input until the
        // completed network turn hands the original native owner to steady state.
        if !a.ready
            && let Some(broker) = &mut a.broker
        {
            match broker.dispatch_record(route, bytes) {
                Ok(Some(disposition)) => return Ok(disposition),
                Ok(None) => {}
                Err(error) => {
                    a.failure = Some(error);
                    a.stop();
                    return Err(());
                }
            }
        }
        self.other.receive(route, bytes)
    }
}
impl StreamingHost {
    /// Receive an explicit control request while this SAME configured capture
    /// process, packetizer, feedback and observation/clock owners keep running.
    /// Input attachment, host view readiness and local approval are prerequisites,
    /// not inferred from decoder completion or a peer's requested target.
    ///
    /// `local` is also serviced during admission refresh and after promotion. It
    /// must return the current qualified target; missing/changed target after
    /// grant stops input before any more application work. The returned Driver
    /// belongs to an independently polled authority task throughout cleanup.
    /// Dropping even an unpolled future fences this original session and worker.
    #[allow(clippy::too_many_arguments)]
    pub fn serve_accepting_control<'a>(
        &'a mut self,
        seat: Seat,
        channels: NegotiatedInput,
        mut local: impl FnMut(LocalControl<'_>) -> Result<Option<Target>, GrantError> + 'a,
        mut nonce: impl FnMut() -> Result<u128, ()> + 'a,
        mut ticket: impl FnMut() -> Option<InputTicketId> + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let prepared = Acquisition::new(self, seat, channels).map(Box::new);
        let fence = Fence {
            control: self.stream.control.clone(),
            native: self.host.native(),
        };
        let operation = Operation { host: self };
        Guarded {
            fence,
            inner: Box::pin(async move {
                let operation = operation;
                let acquisition = prepared?;
                operation
                    .host
                    .serve_inner(
                        &mut nonce,
                        &mut ticket,
                        &mut other,
                        Some(acquisition),
                        &mut local,
                    )
                    .await
            }),
        }
    }
}

#[cfg(test)]
mod tests;
