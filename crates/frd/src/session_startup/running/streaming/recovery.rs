//! Recovery on the original observation host: fence network output immediately,
//! drain at most one native capture, then replace channels without a new sender.
mod handoff;
use super::*;
use crate::media_quic::NegotiatedMedia;
use fr_media::delivery::RecoveryDemand;
use fr_transport::quic::ControlRoutes;
use fr_wire::{decoder::Binding, negotiation::ControlBinding, recovery_request};

/// An inner recovery error must fence authority before its pinned native work
/// is dropped, not merely after the outer serving future returns Ready(Err).
/// Declare this guard AFTER the futures it protects; successful handoff disarms
/// it only after the native/network turn has actually completed.
struct CaptureFence {
    control: ObservationControl,
    complete: bool,
}
impl CaptureFence {
    fn new(control: ObservationControl) -> Self {
        Self {
            control,
            complete: false,
        }
    }
}
impl Drop for CaptureFence {
    fn drop(&mut self) {
        if !self.complete {
            self.control.revoke();
        }
    }
}

impl StreamingHost {
    /// Enable reference recovery using the actual completed media attachments.
    /// This is observation-only and requires positive capability negotiation.
    /// No new sender, native worker, input authority or connection is created.
    pub fn enable_reference_recovery(&mut self, media: NegotiatedMedia) -> Result<(), Error> {
        if self.stream.served || self.recovery.is_some() || !matches!(self.host, Host::Observe(_)) {
            return Err(Error::Order);
        }
        let session = self.host.session()?;
        session.check()?;
        media
            .check_recovery_host(
                &session.opened.transport,
                session.opened.routes,
                session.opened.binding,
                &self.stream.sender,
                Route::Stream(session.opened.routes.inbound),
            )
            .map_err(Error::MediaTransport)?;
        self.recovery = Some(media);
        Ok(())
    }
}

pub(super) async fn serve(
    host: &mut StreamingHost,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
    ticket: &mut impl FnMut() -> Option<InputTicketId>,
    other: &mut impl Services,
) -> Result<(), Error> {
    let mut media = host.recovery.take().ok_or(Error::Order)?;
    loop {
        let demand = round(host, &media, nonce, ticket, other).await?;
        media = Box::pin(handoff::replace(host, media, demand, nonce, ticket, other)).await?;
        // Reports from the old generation cannot establish readiness/freshness
        // for this one. The native capture and pacing histories are not reset.
        install_evidence(host)?;
        host.stream.statistics.recovered_streams =
            host.stream.statistics.recovered_streams.saturating_add(1);
    }
}

fn install_evidence(host: &mut StreamingHost) -> Result<(), Error> {
    let feedback_reports = host.receiver_feedback_reports();
    let presentation_reports = host.presentation_reports();
    let session = host.host.session()?;
    let view = host
        .stream
        .sender
        .feedback_view()
        .map_err(Error::MediaTransport)?;
    host.feedback = Setup::selected(&session.opened.selected, session.opened.binding, view)
        .map_err(Error::ReceiverFeedback)?
        .map(|s| {
            HostFeedback::new(
                s,
                Route::Stream(session.opened.routes.inbound),
                Route::Stream(session.opened.routes.outbound),
            )
        })
        .transpose()
        .map_err(Error::ReceiverFeedback)?;
    if let Some(feedback) = &mut host.feedback {
        feedback.accepted = feedback_reports;
    }
    host.presentation = HostPresentation::attach(
        &session.opened.selected,
        session.opened.binding,
        view,
        &session.opened.transport,
        session.opened.routes.inbound,
        host.stream.control.clone(),
    )
    .map_err(Error::PresentedState)?;
    if let Some(presentation) = &mut host.presentation {
        presentation.accepted = presentation_reports;
        presentation
            .observe(
                host.stream
                    .sender
                    .source_progress()
                    .map_err(Error::MediaTransport)?,
            )
            .map_err(Error::PresentedState)?;
    }
    Ok(())
}

async fn round(
    host: &mut StreamingHost,
    media: &NegotiatedMedia,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
    ticket: &mut impl FnMut() -> Option<InputTicketId>,
    other: &mut impl Services,
) -> Result<RecoveryDemand, Error> {
    let session = host.host.session()?;
    let routes = session.opened.routes;
    let parent = session.opened.binding;
    let stream = &mut host.stream;
    let control = stream.control.clone();
    let (credit, requests) = mpsc::channel(1);
    let (completed, results) = mpsc::channel(1);
    let last_capture = stream
        .sender
        .source_progress()
        .map_err(Error::MediaTransport)?
        .map(|p| p.descriptor.capture_micros);
    let next_capture = last_capture
        .unwrap_or(0)
        .checked_add(
            u64::try_from(stream.policy.capture_interval.as_micros()).map_err(|_| Error::Clock)?,
        )
        .ok_or(Error::Clock)?;
    let mut producer = pin!(produce(
        &mut stream.source,
        &stream.control,
        requests,
        completed
    ));
    let mut services = Admission {
        media,
        routes,
        parent,
        pending: None,
        video: VideoServices {
            control: &stream.control,
            sender: &mut stream.sender,
            statistics: &mut stream.statistics,
            policy: stream.policy,
            capacity: stream.capacity,
            credit,
            results,
            in_flight: None,
            pacing: stream.pacing.as_mut(),
            last_work: None,
            last_capture,
            next_capture,
            input_wake: super::super::input_wake::Wake::default(),
            repair_turn: false,
            feedback: host.feedback.as_mut(),
            presentation: host.presentation.as_mut(),
            other,
        },
    };
    let mut network = pin!(async {
        loop {
            host.host
                .drive(stream.policy.network_turn, nonce, ticket, &mut services)
                .await?;
            if services.video.in_flight.is_none()
                && let Some(demand) = services.pending.take()
            {
                return Ok(demand);
            }
            asupersync::runtime::yield_now().await;
        }
    });
    // Return intentionally ONLY after the network turn ended and the previous
    // capture's result was drained. Dropping the producer then abandons only its
    // idle credit wait, never a pending worker operation or an in-flight QUIC I/O.
    let mut fence = CaptureFence::new(control);
    let result = poll_fn(|task| {
        if let Poll::Ready(result) = producer.as_mut().poll(task) {
            return Poll::Ready(Err(result.err().unwrap_or(Error::Closed)));
        }
        network.as_mut().poll(task)
    })
    .await;
    fence.complete = result.is_ok();
    result
}

struct Admission<'a, S> {
    video: VideoServices<'a, S>,
    media: &'a NegotiatedMedia,
    routes: ControlRoutes,
    parent: ControlBinding,
    pending: Option<RecoveryDemand>,
}
impl<S: Services> Services for Admission<'_, S> {
    fn permitted(&mut self) -> bool {
        self.video.permitted()
            && self.pending.as_ref().is_none_or(|p| {
                self.video
                    .control
                    .check()
                    .is_ok_and(|n| n.as_micros() < p.deadline_micros())
            })
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
    ) -> Result<(), Error> {
        if !self.permitted() {
            return Err(Error::Expired);
        }
        let cx = self.video.control.context();
        let mut failure = None;
        let media = self.media;
        let sender = &mut *self.video.sender;
        let pending = &mut self.pending;
        // Dispatch without mutably borrowing q inside receive_ready. One fixed
        // record stays on the original stream until we have admission ownership.
        let mut bytes = [0; recovery_request::REQUEST_BYTES];
        let mut len = 0;
        let available = std::cell::Cell::new(true);
        q.receive_ready(
            &cx,
            || self.video.control.check().is_ok(),
            |r| available.get() && r == Route::Stream(self.routes.inbound),
            |_, record| {
                if !is_request(record) {
                    return Ok(Disposition::Blocked);
                }
                if record.len() > bytes.len() {
                    return Err(());
                }
                bytes[..record.len()].copy_from_slice(record);
                len = record.len();
                available.set(false);
                Ok(Disposition::Consumed)
            },
        )
        .map_err(Error::Transport)?;
        if len != 0 {
            match media.admit_recovery_request(q, self.routes, self.parent, sender, &bytes[..len]) {
                Ok(Some(demand)) => {
                    if pending.is_some() {
                        return Err(Error::Order);
                    }
                    *pending = Some(demand);
                }
                Ok(None) => {}
                Err(error) => failure = Some(Error::MediaTransport(error)),
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if self.pending.is_some() {
            self.video.other.maintain(q, nonce)?;
            if !self.permitted() {
                return Err(Error::Expired);
            }
            if self.video.in_flight.is_some() {
                match self.video.results.try_recv() {
                    Ok(obsolete) => {
                        drop(obsolete);
                        self.video.in_flight = None;
                    }
                    Err(mpsc::RecvError::Empty) => {}
                    Err(_) => return Err(Error::Closed),
                }
            }
            Ok(())
        } else {
            self.video.maintain(q, nonce)
        }
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        if route == Route::Stream(self.routes.inbound) && is_request(bytes) {
            return Ok(Disposition::Blocked);
        }
        if self.pending.is_some()
            && obsolete_metadata(
                route,
                bytes,
                self.routes,
                self.video.sender.stream_repair_route(),
            )
        {
            return Ok(Disposition::Consumed);
        }
        if self.pending.is_some() {
            self.video.other.receive(route, bytes)
        } else {
            self.video.receive(route, bytes)
        }
    }
}
fn is_request(bytes: &[u8]) -> bool {
    bytes.get(6..8) == Some(&0x0036_u16.to_be_bytes())
}
fn obsolete_metadata(route: Route, bytes: &[u8], routes: ControlRoutes, repair: Route) -> bool {
    route == repair
        || (route == Route::Stream(routes.inbound)
            && (presented::is_report(bytes) || feedback::is_feedback(bytes)))
}

/// Continue consent/renewal and unrelated application work, but discard obsolete
/// advisory reports rather than assigning them to the new generation. Replayed
/// failure requests must still parse against their exact original full binding.
struct Waiting<'a, S> {
    control: &'a ObservationControl,
    until: u64,
    routes: ControlRoutes,
    previous: Binding,
    limits: fr_core::limits::ProtocolLimits,
    repair: Route,
    other: &'a mut S,
}
impl<S: Services> Waiting<'_, S> {
    fn check(&mut self) -> Result<(), Error> {
        let now = self.control.check().map_err(Error::Media)?.as_micros();
        if now >= self.until {
            return Err(Error::Expired);
        }
        if !self.other.permitted() {
            return Err(Error::Authority);
        }
        Ok(())
    }
    fn wait(&mut self, maximum: Duration) -> Result<Duration, Error> {
        self.check()?;
        let now = self.control.check().map_err(Error::Media)?.as_micros();
        let remaining = self
            .until
            .checked_sub(now)
            .filter(|n| *n != 0)
            .ok_or(Error::Expired)?;
        Ok(maximum.min(Duration::from_micros(remaining)))
    }
}
impl<S: Services> Services for Waiting<'_, S> {
    fn permitted(&mut self) -> bool {
        self.check().is_ok()
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
    ) -> Result<(), Error> {
        self.check()?;
        self.other.maintain(q, nonce)?;
        self.check()
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        self.check().map_err(|_| ())?;
        if route == Route::Stream(self.routes.inbound) && is_request(bytes) {
            recovery_request::decode(
                bytes,
                self.previous,
                &self.limits,
                fr_wire::input::InputDirection::ViewerToHost,
                fr_wire::input::InputDelivery::Reliable,
            )
            .map_err(|_| ())?;
            return Ok(Disposition::Consumed);
        }
        if obsolete_metadata(route, bytes, self.routes, self.repair) {
            return Ok(Disposition::Consumed);
        }
        self.other.receive(route, bytes)
    }
}

#[cfg(test)]
mod tests;
