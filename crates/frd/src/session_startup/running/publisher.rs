//! Approved native host bootstrap. Discovery, selected capture and decoder
//! negotiation use their existing owners while the original session stays driven.
use super::{HostSession, Services, StreamingHost};
use crate::{
    display_selection::{DisplaySelection, SelectedDisplay},
    media::{self, ObservationControl, decoder_startup, discovery::DiscoveredSource, streaming},
    media_egress::{Lane, Progress},
    media_quic::{NegotiatedMedia, QuicEgress},
    worker::{Deadline, Launch},
};
use asupersync::cx::Cx;
use fr_media::{delivery::SendPolicy, worker::Configuration};
use fr_transport::quic::{self, ChannelRequest, Disposition, MediaChannel, QuicRecords, Route};
use fr_wire::{
    attachment::{MediaRole, Ticket},
    display::Display,
    negotiation::Role,
};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    Identity,
    Expired,
    Session(super::Error),
    Display(crate::display_selection::Error),
    Media(media::Error),
    Startup(decoder_startup::Error),
    Transport(quic::Error),
    Routes(crate::media_quic::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// One call-time budget, including discovery, remote display choice, native
/// setup and first decode. Shorter native/channel/authority deadlines still win.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub timeout: Duration,
    pub network_turn: Duration,
    pub streaming: streaming::Policy,
    pub adaptive_capture: Option<Duration>,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            network_turn: Duration::from_millis(5),
            streaming: streaming::Policy::default(),
            adaptive_capture: None,
        }
    }
}
impl Policy {
    fn validate(self) -> Result<(), Error> {
        if self.timeout < Duration::from_millis(1)
            || self.timeout > Duration::from_secs(60)
            || self.network_turn < Duration::from_millis(1)
            || self.network_turn > Duration::from_millis(50)
            || self.streaming.capture_interval.is_zero()
            || self.streaming.capture_interval > Duration::from_secs(1)
            || self.streaming.network_turn < Duration::from_millis(1)
            || self.streaming.network_turn > Duration::from_millis(50)
            || !(1..=64).contains(&self.streaming.records_per_turn)
            || self.adaptive_capture.is_some_and(|d| {
                d > Duration::from_millis(200)
                    || d.as_micros() < self.streaming.capture_interval.as_nanos().div_ceil(1000)
            })
        {
            return Err(Error::InvalidConfiguration);
        }
        Ok(())
    }
}
struct Budget {
    control: ObservationControl,
    until: u64,
    turn: Duration,
    failure: std::sync::Mutex<Option<Error>>,
}
impl Budget {
    fn new(control: ObservationControl, policy: Policy) -> Result<Self, Error> {
        policy.validate()?;
        let start = control.check().map_err(Error::Media)?.as_micros();
        let until = start
            .checked_add(
                u64::try_from(policy.timeout.as_micros())
                    .map_err(|_| Error::InvalidConfiguration)?,
            )
            .ok_or(Error::Expired)?;
        Ok(Self {
            control,
            until,
            turn: policy.network_turn,
            failure: std::sync::Mutex::new(None),
        })
    }
    fn fail(&self, error: Error) -> Error {
        // Never retain the error lock while touching shared authority. This
        // fixed slot preserves the primary failure without making startup !Send.
        if let Ok(mut failure) = self.failure.lock() {
            failure.get_or_insert(error);
        }
        self.control.revoke();
        error
    }
    fn remaining(&self) -> Result<Duration, Error> {
        let failure = self
            .failure
            .lock()
            .map_or(Some(Error::Session(super::Error::Closed)), |f| *f);
        if let Some(error) = failure {
            self.control.revoke();
            return Err(error);
        }
        let result = self.control.check().map_err(Error::Media).and_then(|n| {
            self.until
                .checked_sub(n.as_micros())
                .filter(|&n| n != 0)
                .map(Duration::from_micros)
                .ok_or(Error::Expired)
        });
        if result.is_err() {
            self.control.revoke();
        }
        result
    }
    fn wait(&self) -> Result<Duration, Error> {
        Ok(self.turn.min(self.remaining()?))
    }
    fn stage(&self) -> Result<Duration, Error> {
        Ok(Duration::from_secs(2).min(self.remaining()?))
    }
    fn live(&self) -> bool {
        self.remaining().is_ok()
    }
}

/// Selected-display proof and the SAME configured capture process survive the
/// handoff to continuous service. This read-only owner cannot grant input.
pub struct NativePublisher {
    selected: SelectedDisplay,
    display: Display,
    // One stable allocation keeps the large session out of every Poll result.
    // Moving the publication handle never duplicates host or media ownership.
    host: Box<StreamingHost>,
    control: ObservationControl,
}
impl std::fmt::Debug for NativePublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativePublisher([selected native source])")
    }
}
impl NativePublisher {
    pub const fn display(&self) -> Display {
        self.display
    }
    pub fn control(&self) -> ObservationControl {
        self.control.clone()
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.host.worker_id()
    }
    pub fn statistics(&self) -> streaming::Statistics {
        self.host.statistics()
    }
    pub fn pacing(&self) -> Option<&fr_media::pacing::Controller> {
        self.host.pacing()
    }
    pub fn close(&mut self) {
        self.control.revoke();
        self.selected.invalidate();
        self.host.close();
    }
    /// Continue the canonical streaming owner; the nonce source must remain
    /// unpredictable and local. This profile has no input or other side channels.
    pub fn serve<'a>(
        &'a mut self,
        nonce: impl FnMut() -> Result<u128, ()> + 'a,
    ) -> impl Future<Output = Result<(), super::Error>> + 'a {
        self.host.serve(nonce, || None, block)
    }
    pub async fn reap_media(
        &mut self,
        cleanup: &Cx,
        deadline: Deadline,
    ) -> Result<asupersync::process::ExitStatus, crate::worker::Error> {
        self.close();
        self.host.reap_media(cleanup, deadline).await
    }
}
impl Drop for NativePublisher {
    fn drop(&mut self) {
        self.close();
    }
}

impl HostSession {
    /// Publish one already-approved local source to an observer. The capture
    /// executable/display endpoint and codec policy are supplied LOCALLY. Remote
    /// choice selects only an alias returned by that actual discovery process.
    ///
    /// `configure` is called once after explicit choice. It must be bounded and
    /// nonblocking; geometry and native codec validity are checked independently.
    /// `entropy` supplies independent unpredictable values for renewal, channel
    /// IDs and tickets. A collision/zero refuses, never retries indefinitely.
    /// This consumes the session; every failed/abandoned attempt is terminal.
    pub fn publish_display<'a>(
        self,
        launch: Launch,
        policy: Policy,
        configure: impl FnOnce(Display) -> Result<Configuration, ()> + 'a,
        mut entropy: impl FnMut() -> Result<u128, ()> + 'a,
    ) -> impl Future<Output = Result<NativePublisher, Error>> + 'a {
        let control = self.opened.control.clone();
        let budget = Budget::new(control.clone(), policy);
        Attempt {
            control,
            complete: false,
            inner: Box::pin(async move {
                let budget = budget?;
                Box::pin(bootstrap(
                    self,
                    launch,
                    policy,
                    configure,
                    &mut entropy,
                    &budget,
                ))
                .await
            }),
        }
    }
}
struct Attempt<F> {
    control: ObservationControl,
    complete: bool,
    inner: Pin<Box<F>>,
}
impl<F: Future<Output = Result<NativePublisher, Error>>> Future for Attempt<F> {
    type Output = Result<NativePublisher, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Err(e) = this.control.check() {
            this.control.revoke();
            return Poll::Ready(Err(Error::Media(e)));
        }
        let result = this.inner.as_mut().poll(task);
        if let Poll::Ready(ref outcome) = result {
            this.complete = outcome.is_ok();
            if !this.complete {
                this.control.revoke();
            }
        }
        result
    }
}
impl<F> Drop for Attempt<F> {
    fn drop(&mut self) {
        if !self.complete {
            self.control.revoke();
        }
    }
}
#[allow(clippy::unnecessary_wraps)]
fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}

/// Admission refresh may itself span several network turns. Test the original
/// total deadline on EVERY poll, before the canonical driver can submit UDP.
async fn drive(
    host: &mut HostSession,
    budget: &Budget,
    entropy: &mut impl FnMut() -> Result<u128, ()>,
    services: &mut impl Services,
) -> Result<(), Error> {
    let mut network = pin!(host.drive_inner(budget.wait()?, entropy, services));
    poll_fn(|task| {
        if let Err(e) = budget.remaining() {
            return Poll::Ready(Err(e));
        }
        let outcome = network.as_mut().poll(task);
        if let Err(e) = budget.remaining() {
            return Poll::Ready(Err(e));
        }
        outcome.map(|r| r.map_err(|e| budget.fail(Error::Session(e))))
    })
    .await
}
async fn during<T>(
    host: &mut HostSession,
    budget: &Budget,
    entropy: &mut impl FnMut() -> Result<u128, ()>,
    work: impl Future<Output = Result<T, media::Error>>,
) -> Result<T, Error> {
    let mut work = pin!(work);
    loop {
        let mut done = None;
        {
            let mut other = block;
            let mut network = pin!(drive(host, budget, entropy, &mut other));
            poll_fn(|task| {
                if let Err(e) = budget.remaining() {
                    return Poll::Ready(Err(e));
                }
                if done.is_none()
                    && let Poll::Ready(result) = work.as_mut().poll(task)
                {
                    done = Some(result);
                }
                if let Some(Err(e)) = done.as_ref() {
                    budget.control.revoke();
                    return Poll::Ready(Err(Error::Media(*e)));
                }
                // Native SUCCESS is not permission to abandon a healthy pending
                // QUIC turn. Its saved result stays owned until that turn ends.
                network.as_mut().poll(task)
            })
            .await?;
        }
        if let Some(result) = done {
            budget.remaining()?;
            return result.map_err(Error::Media);
        }
    }
}
struct CatalogService<'a> {
    choice: &'a mut DisplaySelection,
    budget: &'a Budget,
}
impl Services for CatalogService<'_> {
    fn permitted(&mut self) -> bool {
        self.budget.live()
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        _: &mut N,
    ) -> Result<(), super::Error> {
        self.choice
            .transmit(q)
            .and_then(|_| self.choice.dispatch(q))
            .map_err(|e| {
                self.budget.fail(Error::Display(e));
                super::Error::Order
            })
    }
    fn receive(&mut self, r: Route, b: &[u8]) -> Result<Disposition, ()> {
        block(r, b)
    }
}
struct ChannelService<'a> {
    channel: &'a mut MediaChannel,
    budget: &'a Budget,
}
impl Services for ChannelService<'_> {
    fn permitted(&mut self) -> bool {
        self.budget.live()
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        _: &mut N,
    ) -> Result<(), super::Error> {
        let cx = self.budget.control.context();
        self.channel.transmit(q, &cx, || self.budget.live())?;
        self.channel.dispatch(q, &cx, || self.budget.live())?;
        self.channel.finish(q, &cx, || self.budget.live())?;
        Ok(())
    }
    fn receive(&mut self, r: Route, b: &[u8]) -> Result<Disposition, ()> {
        block(r, b)
    }
}
struct DecoderService<'a> {
    startup: &'a mut decoder_startup::Host,
    sender: &'a mut QuicEgress,
    budget: &'a Budget,
    configured: bool,
    recovery: bool,
    records: u8,
}
impl Services for DecoderService<'_> {
    fn permitted(&mut self) -> bool {
        self.budget.live()
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        _: &mut N,
    ) -> Result<(), super::Error> {
        let step = (|| -> Result<(), Error> {
            self.budget.remaining()?;
            if !self.configured {
                self.configured = self.startup.transmit(q).map_err(Error::Startup)?;
            }
            self.startup.dispatch(q).map_err(Error::Startup)?;
            if !self.recovery
                && let Some(unit) = self.startup.take_recovery().map_err(Error::Startup)?
            {
                self.sender.enqueue_capture(unit).map_err(Error::Routes)?;
                self.recovery = true;
            }
            if self.recovery {
                let cx = self.budget.control.context();
                for _ in 0..self.records {
                    self.budget.remaining()?;
                    match self
                        .sender
                        .transmit(&cx, q, Lane::Original)
                        .map_err(Error::Routes)?
                    {
                        Progress::Accepted(_) => {}
                        Progress::Idle | Progress::Pending(_) => break,
                    }
                }
            }
            Ok(())
        })();
        step.map_err(|e| {
            self.budget.fail(e);
            super::Error::Order
        })
    }
    fn receive(&mut self, r: Route, b: &[u8]) -> Result<Disposition, ()> {
        block(r, b)
    }
}
async fn attach(
    host: &mut HostSession,
    selected: &mut SelectedDisplay,
    role: MediaRole,
    budget: &Budget,
    entropy: &mut impl FnMut() -> Result<u128, ()>,
) -> Result<MediaChannel, Error> {
    budget.remaining()?;
    // The binding identifier is public; its independent draw reveals no ticket
    // bits. Native reservations reject used IDs; there is no retry/reset path.
    let id =
        u32::try_from(entropy().map_err(|()| budget.fail(Error::Identity))? & u128::from(u32::MAX))
            .map_err(|_| budget.fail(Error::Identity))?;
    let ticket = Ticket(entropy().map_err(|()| budget.fail(Error::Identity))?);
    if ticket.0 == 0 {
        return Err(budget.fail(Error::Identity));
    }
    budget.remaining()?;
    let binding = selected
        .binding(host.io().map_err(|e| budget.fail(Error::Session(e)))?.0, id)
        .map_err(|e| budget.fail(Error::Display(e)))?;
    let mut channel = host
        .offer_media_role(
            ChannelRequest {
                binding,
                ticket,
                timeout: budget.stage()?,
            },
            role,
        )
        .map_err(|e| budget.fail(Error::Session(e)))?;
    while !channel.is_complete() {
        drive(
            host,
            budget,
            entropy,
            &mut ChannelService {
                channel: &mut channel,
                budget,
            },
        )
        .await?;
    }
    Ok(channel)
}
// Keep the linear ownership handoff visible; the bounded native continuation
// is separately pinned so debug builds do not copy it through startup stack frames.
#[allow(clippy::too_many_lines)]
async fn bootstrap(
    mut host: HostSession,
    launch: Launch,
    policy: Policy,
    configure: impl FnOnce(Display) -> Result<Configuration, ()>,
    entropy: &mut impl FnMut() -> Result<u128, ()>,
    budget: &Budget,
) -> Result<NativePublisher, Error> {
    budget.remaining()?;
    if host.selection().role != Role::Observe {
        return Err(Error::InvalidConfiguration);
    }
    for name in [
        fr_wire::display::CAPABILITY,
        fr_wire::attachment::CAPABILITY,
        fr_wire::attachment::DELIVERY_CAPABILITY,
        fr_wire::decoder::CAPABILITY,
    ] {
        if !host
            .selection()
            .capabilities
            .iter()
            .any(|c| c.name == name && c.version == 1)
        {
            return Err(Error::InvalidConfiguration);
        }
    }
    let control = budget.control.clone();
    let source = during(
        &mut host,
        budget,
        entropy,
        DiscoveredSource::start(&control, launch),
    )
    .await?;
    let catalog = source.catalog().map_err(|e| budget.fail(Error::Media(e)))?;
    let mut choice = host
        .select_display(catalog, budget.remaining()?)
        .map_err(|e| budget.fail(Error::Display(e)))?;
    while !choice.is_complete() {
        drive(
            &mut host,
            budget,
            entropy,
            &mut CatalogService {
                choice: &mut choice,
                budget,
            },
        )
        .await?;
    }
    let mut selected = choice
        .finish(host.io().map_err(|e| budget.fail(Error::Session(e)))?.0)
        .map_err(|e| budget.fail(Error::Display(e)))?;
    let display = selected
        .display(host.io().map_err(|e| budget.fail(Error::Session(e)))?.0)
        .map_err(|e| budget.fail(Error::Display(e)))?;
    let configuration =
        configure(display).map_err(|()| budget.fail(Error::InvalidConfiguration))?;
    budget.remaining()?;
    policy
        .streaming
        .validate(configuration.fps)
        .map_err(|e| budget.fail(Error::Media(e)))?;
    let configuring = source
        .configure(
            host.io().map_err(|e| budget.fail(Error::Session(e)))?.0,
            &selected,
            configuration,
        )
        .map_err(|e| budget.fail(Error::Media(e)))?;
    let mut source = Box::pin(during(&mut host, budget, entropy, configuring)).await?;
    let c = attach(
        &mut host,
        &mut selected,
        MediaRole::Configuration,
        budget,
        entropy,
    )
    .await?;
    let r = attach(
        &mut host,
        &mut selected,
        MediaRole::Recovery,
        budget,
        entropy,
    )
    .await?;
    let v = attach(&mut host, &mut selected, MediaRole::Video, budget, entropy).await?;
    let negotiation = host.selection().clone();
    let media = NegotiatedMedia::new(
        host.io().map_err(|e| budget.fail(Error::Session(e)))?.0,
        &negotiation,
        &c,
        &r,
        &v,
    )
    .map_err(|e| budget.fail(Error::Routes(e)))?;
    let initial = during(
        &mut host,
        budget,
        entropy,
        source.capture_if_changed(&control, true),
    )
    .await?;
    let setup = selected
        .decoder_setup(
            host.io().map_err(|e| budget.fail(Error::Session(e)))?.0,
            &media,
            budget.stage()?,
        )
        .map_err(|e| budget.fail(Error::Display(e)))?;
    let mut startup = decoder_startup::Host::new(
        control.clone(),
        host.io().map_err(|e| budget.fail(Error::Session(e)))?.0,
        setup,
        configuration,
        initial,
    )
    .map_err(|e| budget.fail(Error::Startup(e)))?;
    let mut sender = media
        .sender(
            host.io().map_err(|e| budget.fail(Error::Session(e)))?.0,
            control.clone(),
            SendPolicy::default(),
        )
        .map_err(|e| budget.fail(Error::Routes(e)))?;
    {
        let mut service = DecoderService {
            startup: &mut startup,
            sender: &mut sender,
            budget,
            configured: false,
            recovery: false,
            records: policy.streaming.records_per_turn,
        };
        while !service.startup.is_complete() {
            drive(&mut host, budget, entropy, &mut service).await?;
        }
    }
    budget.remaining()?;
    selected
        .check(host.io().map_err(|e| budget.fail(Error::Session(e)))?.0)
        .map_err(|e| budget.fail(Error::Display(e)))?;
    let mut stream = streaming::Stream::new(
        startup,
        source,
        sender,
        host.io().map_err(|e| budget.fail(Error::Session(e)))?.0,
        policy.streaming,
    )
    .map_err(|e| budget.fail(Error::Session(e)))?;
    if let Some(maximum) = policy.adaptive_capture {
        stream
            .enable_adaptive_capture(maximum)
            .map_err(|e| budget.fail(Error::Media(e)))?;
    }
    let host = host
        .into_streaming(stream)
        .map_err(|e| budget.fail(Error::Session(e)))?;
    Ok(NativePublisher {
        selected,
        display,
        host: Box::new(host),
        control,
    })
}

#[cfg(test)]
mod tests;
