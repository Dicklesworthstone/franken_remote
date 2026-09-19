//! One absolute failure budget covers fresh channels, native IDR and peer decode.
use super::*;
use crate::media::{decoder_startup, streaming::Policy};
use crate::media_quic::replacement;
use fr_wire::attachment::Ticket;

// Keep the single linear authority/worker handoff visible; each stage is bounded.
#[allow(clippy::too_many_lines)]
pub(super) async fn replace(
    host: &mut StreamingHost,
    media: NegotiatedMedia,
    demand: RecoveryDemand,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
    ticket: &mut impl FnMut() -> Option<InputTicketId>,
    other: &mut impl Services,
) -> Result<NegotiatedMedia, Error> {
    let until = demand.deadline_micros();
    let control = host.stream.control.clone();
    let cx = control.context();
    let session = host.host.session()?;
    session.check()?;
    let routes = session.opened.routes;
    let parent = session.opened.binding;
    let previous = media
        .check_recovery_host(
            &session.opened.transport,
            routes,
            parent,
            &host.stream.sender,
            Route::Stream(routes.inbound),
        )
        .map_err(Error::MediaTransport)?;
    host.stream
        .sender
        .schedule_recovery(&session.opened.transport, &mut host.stream.source, demand)
        .map_err(Error::MediaTransport)?;
    let mut waiting = Waiting {
        control: &control,
        until,
        routes,
        previous,
        limits: *media.limits().protocol(),
        repair: host.stream.sender.stream_repair_route(),
        other,
    };
    let configuration = host.stream.capture_configuration();
    let policy = host.stream.policy;
    waiting.check()?;
    let mut tickets = [Ticket(0); 3];
    for value in &mut tickets {
        *value = Ticket(nonce().map_err(|()| Error::Order)?);
        waiting.check()?;
    }
    let mut replacement = media
        .begin_replacement(
            &cx,
            &mut session.opened.transport,
            routes,
            parent,
            until,
            Some(tickets),
            || control.check().is_ok(),
        )
        .map_err(Error::ReferenceRecovery)?;
    {
        let mut service = Attaching {
            waiting: &mut waiting,
            replacement: &mut replacement,
        };
        while !service.replacement.is_complete() {
            let wait = service.waiting.wait(policy.network_turn)?;
            host.host.drive(wait, nonce, ticket, &mut service).await?;
        }
    }
    waiting.check()?;
    let q = &mut host.host.session()?.opened.transport;
    let media = replacement
        .finish(&cx, q)
        .map_err(Error::ReferenceRecovery)?;
    let setup = media
        .recover_sender(q, &mut host.stream.sender)
        .map_err(Error::MediaTransport)?;
    // The original source's IDR rate allowance survives generation replacement.
    // Do not substitute force=true, which would bypass queued admission timing.
    while host
        .stream
        .source
        .next_recovery_deadline()
        .is_some_and(|n| control.check().is_ok_and(|now| now < n))
    {
        let wait = waiting.wait(policy.network_turn)?;
        host.host.drive(wait, nonce, ticket, &mut waiting).await?;
    }
    waiting.check()?;
    let update = during(
        &mut host.host,
        policy,
        &mut waiting,
        nonce,
        ticket,
        host.stream.source.capture_if_changed(&control, false),
    )
    .await?;
    waiting.check()?;
    let mut startup = decoder_startup::Host::new(
        control.clone(),
        &host.host.session()?.opened.transport,
        setup,
        configuration,
        update,
    )
    .map_err(Error::DecoderStartup)?;
    {
        let mut service = Decoding {
            waiting: &mut waiting,
            startup: &mut startup,
            sender: &mut host.stream.sender,
            configuration_sent: false,
            recovery_sent: false,
            policy,
            statistics: &mut host.stream.statistics,
        };
        while !service.startup.is_complete() {
            let wait = service.waiting.wait(policy.network_turn)?;
            host.host.drive(wait, nonce, ticket, &mut service).await?;
        }
    }
    waiting.check()?;
    let q = &host.host.session()?.opened.transport;
    let (original, binding) = startup.finish_stream(q).map_err(Error::DecoderStartup)?;
    if !original.same_owner(&control) || binding != media.binding() {
        return Err(Error::Order);
    }
    host.stream
        .sender
        .join_stream(q, &host.stream.source, &control, binding)
        .map_err(Error::MediaTransport)?;
    Ok(media)
}

struct Attaching<'a, 'b, S> {
    waiting: &'a mut Waiting<'b, S>,
    replacement: &'a mut replacement::Replacement,
}
impl<S: Services> Services for Attaching<'_, '_, S> {
    fn permitted(&mut self) -> bool {
        self.waiting.permitted()
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
    ) -> Result<(), Error> {
        self.waiting.maintain(q, nonce)?;
        self.replacement
            .advance(&self.waiting.control.context(), q, || {
                self.waiting.permitted()
            })
            .map_err(Error::ReferenceRecovery)?;
        self.waiting.check()
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        if self.replacement.owns_record(route, bytes) {
            return Ok(Disposition::Blocked);
        }
        self.waiting.receive(route, bytes)
    }
}

async fn during<S: Services>(
    host: &mut Host,
    policy: Policy,
    waiting: &mut Waiting<'_, S>,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
    ticket: &mut impl FnMut() -> Option<InputTicketId>,
    native: impl Future<Output = Result<CaptureUpdate, crate::media::Error>>,
) -> Result<CaptureUpdate, Error> {
    let mut native = pin!(native);
    let mut result = None;
    loop {
        let wait = waiting.wait(policy.network_turn)?;
        let control = waiting.control.clone();
        let mut network = pin!(host.drive(wait, nonce, ticket, waiting));
        let driven = poll_fn(|task| {
            if result.is_none()
                && let Poll::Ready(value) = native.as_mut().poll(task)
            {
                if value.is_err() {
                    control.revoke();
                }
                result = Some(value);
            }
            network.as_mut().poll(task)
        })
        .await;
        // Never drop an unfinished native QUIC operation just because capture
        // completed. Its original bounded turn finishes before consuming output.
        if let Some(value) = result.take() {
            let value = value.map_err(Error::Media)?;
            driven?;
            return Ok(value);
        }
        driven?;
    }
}

struct Decoding<'a, 'b, S> {
    waiting: &'a mut Waiting<'b, S>,
    startup: &'a mut decoder_startup::Host,
    sender: &'a mut QuicEgress,
    configuration_sent: bool,
    recovery_sent: bool,
    policy: Policy,
    statistics: &'a mut Statistics,
}
impl<S: Services> Services for Decoding<'_, '_, S> {
    fn permitted(&mut self) -> bool {
        self.waiting.permitted()
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
    ) -> Result<(), Error> {
        self.waiting.maintain(q, nonce)?;
        self.startup
            .check_transport(q)
            .map_err(Error::DecoderStartup)?;
        if !self.configuration_sent {
            self.configuration_sent = self.startup.transmit(q).map_err(Error::DecoderStartup)?;
        }
        self.startup.dispatch(q).map_err(Error::DecoderStartup)?;
        if !self.recovery_sent
            && let Some(update) = self
                .startup
                .take_recovery()
                .map_err(Error::DecoderStartup)?
        {
            self.sender
                .enqueue_capture(update)
                .map_err(Error::MediaTransport)?;
            self.recovery_sent = true;
            self.statistics.encoded_updates = self.statistics.encoded_updates.saturating_add(1);
        }
        if self.recovery_sent {
            for _ in 0..self.policy.records_per_turn {
                self.waiting.check()?;
                match self
                    .sender
                    .transmit(&self.waiting.control.context(), q, Lane::Original)
                    .map_err(Error::MediaTransport)?
                {
                    Progress::Accepted(_) => {
                        self.statistics.admitted_records =
                            self.statistics.admitted_records.saturating_add(1);
                    }
                    Progress::Idle | Progress::Pending(_) => break,
                }
            }
        }
        self.waiting.check()
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        if matches!(bytes.get(6..8), Some([0, 0x31 | 0x33])) {
            return Ok(Disposition::Blocked);
        }
        self.waiting.receive(route, bytes)
    }
}
