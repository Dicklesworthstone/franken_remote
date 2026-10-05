//! Recovery on the original observation host: fence network output immediately,
//! drain at most one native capture, then replace channels without a new sender.
mod handoff;
#[allow(clippy::wildcard_imports)]
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
    /// Requires positive capability negotiation. No new sender, native worker,
    /// input authority or connection is created.
    pub fn enable_reference_recovery(&mut self, media: NegotiatedMedia) -> Result<(), Error> {
        self.retain_media(media)?;
        self.enable_retained_recovery()
    }
    /// Reference recovery on the media this stream already retains: a
    /// controlled share's cursor lanes and its recovery share one owner.
    /// A recovery suspends the grant's input (its fence is a stale view, plan
    /// 11.3) and moves it to the recovered generation (plan 12.3).
    pub fn enable_retained_recovery(&mut self) -> Result<(), Error> {
        if self.stream.served || self.reference_recovery {
            return Err(Error::Order);
        }
        let session = self.host.session()?;
        session.check()?;
        self.media
            .as_ref()
            .ok_or(Error::Order)?
            .check_recovery_host(
                &session.opened.transport,
                session.opened.routes,
                session.opened.binding,
                &self.stream.sender,
                Route::Stream(session.opened.routes.inbound),
            )
            .map_err(Error::MediaTransport)?;
        self.reference_recovery = true;
        Ok(())
    }
}

/// One reference recovery of this stream, after its round drained the
/// in-flight capture: replace the media attachments, advance the authority's
/// recovery generation so input decided on older pictures is stale (plan
/// 12.3), then rebuild advisory evidence for the new generation.
pub(super) async fn recover(
    host: &mut StreamingHost,
    demand: RecoveryDemand,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
    ticket: &mut impl FnMut() -> Option<InputTicketId>,
    other: &mut impl Services,
    acquisition: &mut Option<Box<acquisition::Acquisition>>,
    local: &mut impl FnMut(
        acquisition::LocalControl<'_>,
    )
        -> Result<Option<fr_wire::control::Target>, crate::input_quic::grant::Error>,
) -> Result<(), Error> {
    let media = host.media.take().ok_or(Error::Order)?;
    let media = Box::pin(handoff::replace(
        host,
        media,
        demand,
        nonce,
        ticket,
        other,
        acquisition,
        local,
    ))
    .await?;
    // Before any report of the new generation can revive readiness or a
    // ticket can name it. The recovery fence already suspended input.
    host.stream
        .control
        .advance_recovery(media.binding().recovery)
        .map_err(Error::Media)?;
    host.media = Some(media);
    // Reports from the old generation cannot establish readiness/freshness
    // for this one. The native capture and pacing histories are not reset.
    install_evidence(host)?;
    host.stream.statistics.recovered_streams =
        host.stream.statistics.recovered_streams.saturating_add(1);
    Ok(())
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

pub(super) fn expired_original(error: &crate::media_quic::Error) -> bool {
    matches!(
        error,
        crate::media_quic::Error::Media(crate::media::Error::Send(
            fr_media::delivery::SendError::OriginalExpired
        ))
    )
}

/// A local failure is admitted immediately, but must not replace a healthy
/// peer's decoder unannounced. Retain its original budget while the reliable
/// Failed progress record elicits the peer's ordinary bound recovery request.
pub(super) struct LocalFailure {
    demand: RecoveryDemand,
    progress: fr_wire::Progress,
    notified: bool,
}

/// Every round's services: without negotiated recovery (`media` absent) a
/// plain pass-through to the video services.
pub(super) struct Admission<'a, S> {
    pub(super) video: VideoServices<'a, S>,
    pub(super) media: Option<&'a NegotiatedMedia>,
    pub(super) routes: ControlRoutes,
    pub(super) parent: ControlBinding,
    pub(super) pending: Option<RecoveryDemand>,
    pub(super) local_failure: Option<LocalFailure>,
}
impl<S: Services> Admission<'_, S> {
    fn recovering(&self) -> bool {
        self.pending.is_some() || self.local_failure.is_some()
    }
    fn maintain_video<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
        media: &NegotiatedMedia,
    ) -> Result<(), Error> {
        // Keep only the last real descriptor, not its payload. The expired
        // enqueue fences/clears the cache before returning its error.
        let previous = self
            .video
            .sender
            .source_progress()
            .map_err(Error::MediaTransport)?;
        match self.video.maintain(q, nonce) {
            Err(Error::MediaTransport(error)) if expired_original(&error) => {
                let progress = previous.ok_or(Error::MediaTransport(error))?;
                let demand = media
                    .admit_sender_failure(q, self.routes, self.parent, self.video.sender)
                    .map_err(Error::MediaTransport)?;
                self.local_failure = Some(LocalFailure {
                    demand,
                    progress,
                    notified: false,
                });
                Ok(())
            }
            result => result,
        }
    }
    fn maintain_failure<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
        media: &NegotiatedMedia,
    ) -> Result<(), Error> {
        self.video.other.maintain(q, nonce)?;
        if !self.permitted() {
            return Err(Error::Expired);
        }
        if let Some(local) = &mut self.local_failure
            && !local.notified
        {
            local.notified = media
                .notify_sender_failure(
                    &self.video.control.context(),
                    q,
                    self.video.sender,
                    local.progress,
                    local.demand.deadline_micros(),
                )
                .map_err(Error::MediaTransport)?;
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
    }
}
impl<S: Services> Services for Admission<'_, S> {
    // Not a defaulted no-op: a controlled session's collected input wakes an
    // idle capture through the video services.
    fn input_submitted(&mut self, at_us: u64) {
        self.video.input_submitted(at_us);
    }
    fn permitted(&mut self) -> bool {
        self.video.permitted()
            && self
                .pending
                .as_ref()
                .or_else(|| self.local_failure.as_ref().map(|local| &local.demand))
                .is_none_or(|p| {
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
        let Some(media) = self.media else {
            return self.video.maintain(q, nonce);
        };
        if !self.permitted() {
            return Err(Error::Expired);
        }
        let cx = self.video.control.context();
        let mut failure = None;
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
                    if pending.is_some() || self.local_failure.is_some() {
                        return Err(Error::Order);
                    }
                    *pending = Some(demand);
                }
                Ok(None) => {
                    // Only an actually received and fully validated peer request
                    // may release a locally admitted demand to the handoff.
                    if let Some(local) = self.local_failure.take() {
                        if pending.is_some() {
                            return Err(Error::Order);
                        }
                        *pending = Some(local.demand);
                    }
                }
                Err(error) => failure = Some(Error::MediaTransport(error)),
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if self.recovering() {
            self.maintain_failure(q, nonce, media)
        } else {
            self.maintain_video(q, nonce, media)
        }
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        if self.media.is_none() {
            return self.video.receive(route, bytes);
        }
        if route == Route::Stream(self.routes.inbound) && is_request(bytes) {
            return Ok(Disposition::Blocked);
        }
        if self.recovering()
            && obsolete_metadata(
                route,
                bytes,
                self.routes,
                self.video.sender.stream_repair_route(),
            )
        {
            return Ok(Disposition::Consumed);
        }
        if self.recovering() {
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
