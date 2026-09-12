//! Capture and canonical session service run concurrently. One capture credit
//! covers the only pending native result; packetization retains its existing
//! exact pending record. Neither codec waits nor admission refresh block the
//! other owner's maintenance. The native input Driver remains independent.
use super::{ControlledHost, Error, HostSession, Services, now};
use crate::{
    input_watchdog::{Control, StopReason},
    media::receiver_feedback::{self as feedback, HostFeedback, Setup},
    media::{
        CaptureSource, CaptureUpdate, ObservationControl,
        streaming::{Statistics, Stream},
    },
    media_egress::{Lane, Progress},
    media_quic::QuicEgress,
};
use asupersync::{channel::mpsc, cx::Cx};
use fr_core::ids::InputTicketId;
use fr_media::pacing::{self, Availability, Observation, Sample};
use fr_transport::quic::{Disposition, QuicRecords, Route};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};

// Exactly one bounded session is retained; boxing would add a second ownership allocation.
#[allow(clippy::large_enum_variant)]
enum Host {
    Observe(HostSession),
    Control(ControlledHost),
}
impl Host {
    fn session(&mut self) -> &mut HostSession {
        match self {
            Self::Observe(h) => h,
            Self::Control(h) => &mut h.session,
        }
    }
    fn native(&self) -> Option<Control> {
        match self {
            Self::Observe(_) => None,
            Self::Control(h) => Some(h.control()),
        }
    }
    fn close(&mut self) {
        match self {
            Self::Observe(h) => h.close(),
            Self::Control(h) => h.close(),
        }
    }
    async fn drive(
        &mut self,
        wait: Duration,
        nonce: &mut impl FnMut() -> Result<u128, ()>,
        ticket: &mut impl FnMut() -> Option<InputTicketId>,
        services: &mut impl Services,
    ) -> Result<(), Error> {
        match self {
            Self::Observe(h) => h.drive_inner(wait, nonce, services).await,
            Self::Control(h) => h.drive_services(wait, nonce, ticket, services).await,
        }
    }
}
/// Continuous, single-view native video on the SAME already-running session.
/// The stream is already configured and its bootstrap acknowledged. Control,
/// when present, must already come from the existing initial-grant broker.
pub struct StreamingHost {
    host: Host,
    stream: Stream,
    feedback: Option<HostFeedback>,
}
impl HostSession {
    pub fn into_streaming(self, stream: Stream) -> Result<StreamingHost, Error> {
        StreamingHost::new(Host::Observe(self), stream)
    }
}
impl ControlledHost {
    pub fn into_streaming(self, stream: Stream) -> Result<StreamingHost, Error> {
        StreamingHost::new(Host::Control(self), stream)
    }
}
impl StreamingHost {
    fn new(mut host: Host, mut stream: Stream) -> Result<Self, Error> {
        let admitted = (|| {
            if host.native().is_some_and(|c| c.is_stopped()) {
                return Err(Error::Closed);
            }
            let session = host.session();
            session.check()?;
            if !session.opened.control.same_owner(&stream.control) || stream.served {
                return Err(Error::Order);
            }
            stream
                .sender
                .stream_check(&session.opened.transport)
                .map_err(Error::MediaTransport)?;
            let setup = Setup::selected(
                &session.opened.selected,
                session.opened.binding,
                stream
                    .sender
                    .feedback_view()
                    .map_err(Error::MediaTransport)?,
            )
            .map_err(Error::ReceiverFeedback)?;
            setup
                .map(|s| {
                    HostFeedback::new(
                        s,
                        Route::Stream(session.opened.routes.inbound),
                        Route::Stream(session.opened.routes.outbound),
                    )
                })
                .transpose()
                .map_err(Error::ReceiverFeedback)
        })();
        let feedback = match admitted {
            Ok(feedback) => feedback,
            Err(error) => {
                host.close();
                stream.close();
                return Err(error);
            }
        };
        Ok(Self {
            host,
            stream,
            feedback,
        })
    }
    pub fn statistics(&self) -> Statistics {
        self.stream.statistics()
    }
    pub fn enable_adaptive_capture(&mut self, maximum: Duration) -> Result<(), Error> {
        self.stream
            .enable_adaptive_capture(maximum)
            .map_err(Error::Media)
    }
    pub fn pacing(&self) -> Option<&pacing::Controller> {
        self.stream.pacing()
    }
    /// Accepted advisory reports, not rendered or visible frames.
    pub fn receiver_feedback_reports(&self) -> u64 {
        self.feedback.as_ref().map_or(0, |f| f.accepted)
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.stream.worker_id()
    }
    /// Local stop is terminal. For control sessions fence input FIRST; native
    /// cleanup/late receipts remain with its original independently polled Driver.
    pub fn close(&mut self) {
        self.host.close();
        self.stream.close();
    }
    pub async fn reap_media(
        &mut self,
        cx: &Cx,
        deadline: crate::worker::Deadline,
    ) -> Result<asupersync::process::ExitStatus, crate::worker::Error> {
        self.close();
        self.stream.reap(cx, deadline).await
    }
    pub fn collect_after_close(
        &mut self,
    ) -> Result<Option<crate::input_agent::InputReply>, crate::input_quic::Error> {
        match &mut self.host {
            Host::Control(h) => h.collect_after_close(),
            Host::Observe(_) => Ok(None),
        }
    }
    /// Runs until local revoke, cancellation, native failure or protocol error.
    /// This does not spawn detached tasks. The caller keeps any native input
    /// Driver running independently and supplies the existing unpredictable ID
    /// sources. Unrelated application dispatch must be bounded and nonblocking.
    /// Dropping even an unpolled serve fences authority before foreign cleanup;
    /// the same source and stream cannot be resumed after an uncertain operation.
    pub fn serve<'a>(
        &'a mut self,
        mut nonce: impl FnMut() -> Result<u128, ()> + 'a,
        mut ticket: impl FnMut() -> Option<InputTicketId> + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let fence = Fence {
            control: self.stream.control.clone(),
            native: self.host.native(),
        };
        let operation = Operation { host: self };
        Guarded {
            fence,
            inner: Box::pin(async move {
                let operation = operation;
                operation
                    .host
                    .serve_inner(&mut nonce, &mut ticket, &mut other)
                    .await
            }),
        }
    }
    async fn serve_inner(
        &mut self,
        nonce: &mut impl FnMut() -> Result<u128, ()>,
        ticket: &mut impl FnMut() -> Option<InputTicketId>,
        other: &mut impl Services,
    ) -> Result<(), Error> {
        if self.stream.served {
            return Err(Error::Closed);
        }
        self.stream.served = true;
        let fence = Fence {
            control: self.stream.control.clone(),
            native: self.host.native(),
        };
        let stream = &mut self.stream;
        let (credit, requests) = mpsc::channel(1);
        let (completed, results) = mpsc::channel(1);
        let mut producer = pin!(produce(
            &mut stream.source,
            &stream.control,
            requests,
            completed
        ));
        let mut services = VideoServices {
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
            last_capture: None,
            next_capture: 0,
            input_wake: super::input_wake::Wake::default(),
            repair_turn: false,
            feedback: self.feedback.as_mut(),
            other,
        };
        let mut network = pin!(async {
            loop {
                self.host
                    .drive(stream.policy.network_turn, nonce, ticket, &mut services)
                    .await?;
                asupersync::runtime::yield_now().await;
            }
        });
        // Neither successful capture nor a ready refresh wins a cancellation
        // race with an in-flight QUIC drive. Only terminal failure exits the join.
        poll_fn(|task| {
            if let Poll::Ready(result) = producer.as_mut().poll(task) {
                fence.stop();
                return Poll::Ready(result);
            }
            let result = network.as_mut().poll(task);
            if result.is_ready() {
                fence.stop();
            }
            result
        })
        .await
    }
}
impl Drop for StreamingHost {
    fn drop(&mut self) {
        self.close();
    }
}
struct Operation<'a> {
    host: &'a mut StreamingHost,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        self.host.close();
    }
}
struct Fence {
    control: ObservationControl,
    native: Option<Control>,
}
impl Fence {
    fn stop(&self) {
        if let Some(control) = &self.native {
            control.stop(StopReason::LocalRevoke);
        }
        self.control.revoke();
    }
}
struct Guarded<F> {
    fence: Fence,
    inner: Pin<Box<F>>,
}
impl<F: Future> Future for Guarded<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this.inner.as_mut().poll(task);
        if result.is_ready() {
            this.fence.stop();
        }
        result
    }
}
impl<F> Drop for Guarded<F> {
    fn drop(&mut self) {
        self.fence.stop();
    }
}

async fn produce(
    source: &mut CaptureSource,
    control: &ObservationControl,
    mut requests: mpsc::Receiver<()>,
    completed: mpsc::Sender<CaptureUpdate>,
) -> Result<(), Error> {
    let cx = control.context();
    loop {
        requests.recv(&cx).await.map_err(|_| Error::Closed)?;
        control.check().map_err(Error::Media)?;
        let update = source
            .capture_if_changed(control, false)
            .await
            .map_err(Error::Media)?;
        // Exactly one issued credit exists; no second native operation can run
        // until this result has transferred to the canonical cache.
        completed.try_send(update).map_err(|_| Error::Order)?;
    }
}
struct VideoServices<'a, S> {
    control: &'a ObservationControl,
    sender: &'a mut QuicEgress,
    statistics: &'a mut Statistics,
    policy: crate::media::streaming::Policy,
    capacity: usize,
    credit: mpsc::Sender<()>,
    results: mpsc::Receiver<CaptureUpdate>,
    in_flight: Option<u64>,
    pacing: Option<&'a mut pacing::Controller>,
    last_work: Option<(u64, u64)>,
    last_capture: Option<u64>,
    next_capture: u64,
    input_wake: super::input_wake::Wake,
    repair_turn: bool,
    feedback: Option<&'a mut HostFeedback>,
    other: &'a mut S,
}
impl<S> VideoServices<'_, S> {
    fn collect_capture(&mut self, cx: &Cx) -> Result<Option<Observation>, Error> {
        let mut observation = None;
        if let Some(started) = self.in_flight {
            match self.results.try_recv() {
                Ok(update) => {
                    let unchanged = update.is_unchanged();
                    let observed = update.observed_micros();
                    self.sender
                        .enqueue_capture(update)
                        .map_err(Error::MediaTransport)?;
                    self.in_flight = None;
                    let collected = now(cx)?;
                    self.last_work = Some((
                        collected,
                        collected.checked_sub(started).ok_or(Error::Clock)?,
                    ));
                    observation = Some(Observation {
                        at_us: observed,
                        changed: !unchanged,
                    });
                    let count = if unchanged {
                        &mut self.statistics.unchanged_observations
                    } else {
                        &mut self.statistics.encoded_updates
                    };
                    *count = count.saturating_add(1);
                }
                Err(mpsc::RecvError::Empty) => {}
                Err(_) => return Err(Error::Closed),
            }
        }
        Ok(observation)
    }
    fn capture_interval(
        &mut self,
        current: u64,
        send: Availability,
        credit: bool,
        observation: Option<Observation>,
    ) -> Result<u64, Error> {
        let interval = if let Some(controller) = &mut self.pacing {
            let elapsed = self.in_flight.map(|start| current - start);
            let completed = self
                .last_work
                .filter(|&(at, _)| current - at <= pacing::SOURCE_EVIDENCE_US)
                .map(|(_, elapsed)| elapsed);
            let source_work_us = elapsed.into_iter().chain(completed).max();
            let sample = Sample {
                now_us: current,
                source_work_us,
                send,
                capture_credit: if credit {
                    Availability::Ready
                } else {
                    Availability::Blocked
                },
                observation,
            };
            let report = if let Some(feedback) = &mut self.feedback {
                let load = feedback
                    .evidence(current)
                    .map_err(Error::ReceiverFeedback)?;
                controller.update_with_receiver(sample, load)
            } else {
                controller.update(sample)
            }
            .map_err(|error| Error::Media(crate::media::Error::Pacing(error)))?;
            if report.interval_us != report.previous_interval_us
                && let Some(last) = self.last_capture
            {
                // Reschedule only the NEXT raw admission. Already queued or
                // executing work and every encoded deadline remain unchanged.
                self.next_capture = last.checked_add(report.interval_us).ok_or(Error::Clock)?;
            }
            report.interval_us
        } else {
            u64::try_from(self.policy.capture_interval.as_micros()).map_err(|_| Error::Clock)?
        };
        Ok(interval)
    }
}
impl<S: Services> Services for VideoServices<'_, S> {
    fn input_submitted(&mut self, at_us: u64) {
        self.input_wake.note(at_us);
        self.other.input_submitted(at_us);
    }
    fn permitted(&mut self) -> bool {
        self.control.check().is_ok() && self.other.permitted()
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
    ) -> Result<(), Error> {
        self.other.maintain(q, nonce)?;
        if !self.permitted() {
            return Err(Error::Authority);
        }
        self.sender.stream_check(q).map_err(Error::MediaTransport)?;
        let cx = self.control.context();
        let repair = self.sender.stream_repair_route();
        let mut failure = None;
        q.receive_ready(
            &cx,
            || self.control.check().is_ok(),
            |route| route == repair,
            |route, bytes| match self.sender.repair(route, bytes) {
                Ok(_) => {
                    self.statistics.repair_requests =
                        self.statistics.repair_requests.saturating_add(1);
                    Ok(Disposition::Consumed)
                }
                Err(error) => {
                    failure = Some(error);
                    Err(())
                }
            },
        )
        .map_err(|error| failure.map_or(Error::Transport(error), Error::MediaTransport))?;
        let observation = self.collect_capture(&cx)?;
        let mut idle = 0;
        let mut send = Availability::Ready;
        for _ in 0..self.policy.records_per_turn {
            if !self.permitted() {
                return Err(Error::Authority);
            }
            let lane = if self.repair_turn {
                Lane::Repair
            } else {
                Lane::Original
            };
            self.repair_turn = !self.repair_turn;
            match self
                .sender
                .transmit(&cx, q, lane)
                .map_err(Error::MediaTransport)?
            {
                Progress::Pending(_) => {
                    send = Availability::Blocked;
                    break;
                }
                Progress::Accepted(_) => {
                    idle = 0;
                    self.statistics.admitted_records =
                        self.statistics.admitted_records.saturating_add(1);
                }
                Progress::Idle => {
                    idle += 1;
                    if idle == 2 {
                        break;
                    }
                }
            }
        }
        let current = now(&cx)?;
        if let Some(feedback) = &mut self.feedback {
            feedback
                .service(q, &cx, current)
                .map_err(Error::ReceiverFeedback)?;
        }
        let credit = self.sender.stream_credit(self.capacity);
        let interval = self.capture_interval(current, send, credit, observation)?;
        let wake = self.input_wake.due(
            current,
            self.last_capture,
            self.in_flight.is_some(),
            self.pacing.as_deref(),
        )?;
        if self.in_flight.is_none() && (current >= self.next_capture || wake) && credit {
            self.credit.try_send(()).map_err(|_| Error::Order)?;
            if current < self.next_capture {
                self.statistics.input_wake_captures =
                    self.statistics.input_wake_captures.saturating_add(1);
            }
            self.input_wake.admitted();
            self.in_flight = Some(current);
            self.last_capture = Some(current);
            self.next_capture = current.checked_add(interval).ok_or(Error::Clock)?;
        }
        Ok(())
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        if feedback::is_feedback(bytes) {
            let feedback = self.feedback.as_mut().ok_or(())?;
            let now = self.control.check().map_err(|_| ())?.as_micros();
            feedback.receive(route, bytes, now).map_err(|_| ())?;
            Ok(Disposition::Consumed)
        } else if route == self.sender.stream_repair_route() {
            Ok(Disposition::Blocked)
        } else {
            self.other.receive(route, bytes)
        }
    }
}

#[cfg(test)]
pub(in crate::session_startup) mod tests;

#[cfg(test)]
mod native;
