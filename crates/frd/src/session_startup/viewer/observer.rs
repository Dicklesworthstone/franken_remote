//! One native observer bootstrap, from approved startup to continuous receiving.
//! The selected display, connection and decoder are transferred, never recreated.
use super::{Viewer, ViewerSession, now, streaming};
use crate::input_quic::NegotiatedInput;
use crate::session_startup::native_control;
use crate::{
    display_selection::{DisplaySelection, SelectedDisplay},
    media::{PresentationReceipt, decoder_startup},
    media_quic::NegotiatedMedia,
    worker::{Deadline, Launch},
};
use asupersync::{cx::Cx, types::CancelKind};
use fr_client::startup::ApprovalNotice;
use fr_media::delivery::{BudgetUsage, ReceivePolicy};
use fr_media::freshness::ClockPolicy;
use fr_transport::quic::{self, Disposition, MediaChannel, Route};
use fr_wire::{
    Kind,
    attachment::{self, MediaRole, Message},
    display::{Catalog, Display},
    input::{InputDelivery, InputDirection},
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
    Expired,
    Application,
    Order,
    Allocation,
    Session(super::Error),
    Display(crate::display_selection::Error),
    Transport(quic::Error),
    Wire(fr_wire::WireError),
    Media(crate::media_quic::Error),
    Decoder(decoder_startup::Error),
    Streaming(streaming::Error),
    Input(crate::input_quic::Error),
    Clock(crate::media::clock::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// One total budget includes unpolled time, approval, local choice, attachments,
/// configuration and first decode. Existing shorter stage deadlines still apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub timeout: Duration,
    pub network_turn: Duration,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            network_turn: Duration::from_millis(5),
        }
    }
}
impl Policy {
    fn validate(self) -> Result<(), Error> {
        if self.timeout < Duration::from_millis(1)
            || self.timeout > Duration::from_secs(60)
            || self.network_turn < Duration::from_millis(1)
            || self.network_turn > Duration::from_millis(50)
        {
            return Err(Error::InvalidConfiguration);
        }
        Ok(())
    }
}
struct Budget {
    cx: Cx,
    until: u64,
    turn: Duration,
}
impl Budget {
    fn new(cx: Cx, policy: Policy) -> Result<Self, Error> {
        policy.validate()?;
        let us =
            u64::try_from(policy.timeout.as_micros()).map_err(|_| Error::InvalidConfiguration)?;
        let until = now(&cx)
            .map_err(Error::Session)?
            .checked_add(us)
            .ok_or(Error::Expired)?;
        Ok(Self {
            cx,
            until,
            turn: policy.network_turn,
        })
    }
    fn remaining(&self) -> Result<Duration, Error> {
        let result = now(&self.cx).map_err(Error::Session).and_then(|n| {
            self.until
                .checked_sub(n)
                .filter(|&n| n != 0)
                .map(Duration::from_micros)
                .ok_or(Error::Expired)
        });
        if result.is_err() {
            self.cx.cancel_fast(CancelKind::User);
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

/// Original display proof stays owned for the complete viewing lifetime. The
/// first native completion is metadata, not visibility or an input grant.
/// No raw transport, decoder worker or mutable selection escapes this owner.
pub struct NativeObserver {
    selected: SelectedDisplay,
    display: Display,
    initial: streaming::Presentation,
    // Keep the large receiver/transport owner out of nested bootstrap Poll results.
    // One stable allocation moves the same session; it does not clone its state.
    viewer: Box<streaming::StreamingViewer>,
    cx: Cx,
    input: Option<NegotiatedInput>,
    parent: fr_wire::negotiation::ControlBinding,
    view: fr_wire::decoder::Binding,
}
impl std::fmt::Debug for NativeObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeObserver([selected display and original decoder])")
    }
}
impl NativeObserver {
    pub const fn display(&self) -> Display {
        self.display
    }
    pub const fn initial_presentation(&self) -> streaming::Presentation {
        self.initial
    }
    /// Request metadata uses this actual selected display, not caller-guessed
    /// channel IDs or generations. Capabilities remain a request, not permission.
    pub fn control_request(
        &self,
        sequence: u64,
        capabilities: fr_core::input_submission::Capabilities,
    ) -> Result<fr_wire::control::Request, Error> {
        now(&self.cx).map_err(Error::Session)?;
        Ok(fr_wire::control::Request {
            parent: self.parent,
            sequence,
            target: native_control::target(self.display, self.view, capabilities)
                .map_err(Error::Wire)?,
        })
    }
    pub fn presentation_reports(&self) -> u64 {
        self.viewer.presentation_reports()
    }
    pub fn last_result(&mut self) -> Option<fr_client::input::ResultEvent> {
        self.viewer.last_result()
    }
    /// Continue the explicit control-capable bootstrap without exposing or
    /// replacing the connection, input attachment, selected display or decoder.
    /// Visibility and mapping still require independent local UI confirmation.
    /// The grant request is constructed NOW, including its fixed timeout.
    pub fn serve_requesting_control<'a>(
        &'a mut self,
        sequence: u64,
        capabilities: fr_core::input_submission::Capabilities,
        policy: fr_client::input::Policy,
        ui: impl FnMut(streaming::ControlState<'_>, Option<streaming::Presentation>) -> Result<(), ()>
        + 'a,
        result: impl FnMut(fr_client::input::ResultEvent) + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let cx = self.cx.clone();
        let request = self.control_request(sequence, capabilities);
        let future = request.and_then(|request| {
            self.input
                .take()
                .ok_or(Error::InvalidConfiguration)
                .map(|input| {
                    self.viewer.serve_requesting_control(
                        input,
                        request,
                        policy,
                        ui,
                        result,
                        |_, _| Err(()),
                    )
                })
        });
        Attempt {
            cx,
            complete: false,
            inner: Box::pin(async move { future?.await.map_err(Error::Streaming) }),
        }
    }
    pub fn control(&self) -> streaming::StreamingViewerControl {
        self.viewer.control()
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.viewer.worker_id()
    }
    pub fn statistics(&self) -> streaming::Statistics {
        self.viewer.statistics()
    }
    pub fn budget_usage(&self) -> BudgetUsage {
        self.viewer.budget_usage()
    }
    pub fn close(&mut self) {
        self.viewer.close();
        self.selected.invalidate();
    }
    /// The callback is bounded/nonblocking; a native compositor submission must
    /// not be relabelled visible. This API cannot acquire or synthesize control.
    pub fn serve<'a>(
        &'a mut self,
        mut ui: impl FnMut(Option<streaming::Presentation>) -> Result<(), ()> + 'a,
    ) -> impl Future<Output = Result<(), streaming::Error>> + 'a {
        self.viewer.serve(
            move |input, frame| {
                if input.is_some() {
                    return Err(());
                }
                ui(frame)
            },
            |_| {},
            |_, _| Err(()),
        )
    }
    pub async fn reap_media(
        &mut self,
        cleanup: &Cx,
        deadline: Deadline,
    ) -> Result<asupersync::process::ExitStatus, crate::media::Error> {
        self.close();
        self.viewer.reap_media(cleanup, deadline).await
    }
}
impl Drop for NativeObserver {
    fn drop(&mut self) {
        self.close();
    }
}

impl Viewer {
    /// Complete the existing negotiation (including optional host-side consent),
    /// require an explicit local full-display choice, attach the three media
    /// roles, validate HEVC, configure the real worker and decode its first IDR.
    /// `choose` returns None while local UI is pending; it is called again between
    /// bounded network turns. `approval` is a notification, NEVER an approval RPC.
    /// Both callbacks must be nonblocking. Dropping an unpolled attempt is terminal.
    pub fn observe<'a>(
        self,
        launch: Launch,
        policy: Policy,
        choose: impl FnMut(&Catalog) -> Result<Option<u128>, ()> + 'a,
        approval: impl FnMut(ApprovalNotice) -> Result<(), ()> + 'a,
    ) -> impl Future<Output = Result<NativeObserver, Error>> + 'a {
        self.observe_native(launch, policy, None, choose, approval)
    }
    /// Prepare control after the SAME approved startup, display selection and
    /// first native decode. Positive capability selection is required; this
    /// does not upgrade observation intent or send a control request yet.
    pub fn observe_for_control<'a>(
        self,
        launch: Launch,
        policy: Policy,
        clock: ClockPolicy,
        choose: impl FnMut(&Catalog) -> Result<Option<u128>, ()> + 'a,
        approval: impl FnMut(ApprovalNotice) -> Result<(), ()> + 'a,
    ) -> impl Future<Output = Result<NativeObserver, Error>> + 'a {
        self.observe_native(launch, policy, Some(clock), choose, approval)
    }
    fn observe_native<'a>(
        mut self,
        launch: Launch,
        policy: Policy,
        clock: Option<ClockPolicy>,
        mut choose: impl FnMut(&Catalog) -> Result<Option<u128>, ()> + 'a,
        mut approval: impl FnMut(ApprovalNotice) -> Result<(), ()> + 'a,
    ) -> impl Future<Output = Result<NativeObserver, Error>> + 'a {
        let cx = self.cx.clone();
        let budget = Budget::new(cx.clone(), policy);
        Attempt {
            cx,
            complete: false,
            inner: Box::pin(async move {
                let budget = budget?;
                let mut notified = None;
                while !self.is_complete() {
                    self.drive(budget.wait()?).await.map_err(Error::Session)?;
                    if let Some(notice) = self.approval()
                        && notified != Some(notice)
                    {
                        approval(notice).map_err(|()| Error::Application)?;
                        notified = Some(notice);
                        budget.remaining()?;
                    }
                }
                Box::pin(bootstrap(
                    self.finish().map_err(Error::Session)?,
                    launch,
                    &budget,
                    &mut choose,
                    clock,
                ))
                .await
            }),
        }
    }
}
impl ViewerSession {
    /// The same path for applications that already completed shared startup.
    /// This never changes the negotiated intent or requests an input lease.
    pub fn observe<'a>(
        self,
        launch: Launch,
        policy: Policy,
        choose: impl FnMut(&Catalog) -> Result<Option<u128>, ()> + 'a,
    ) -> impl Future<Output = Result<NativeObserver, Error>> + 'a {
        self.observe_native(launch, policy, None, choose)
    }
    /// The same explicit path for an already approved session. The existing
    /// session must not already own a clock: there is exactly one estimator.
    pub fn observe_for_control<'a>(
        self,
        launch: Launch,
        policy: Policy,
        clock: ClockPolicy,
        choose: impl FnMut(&Catalog) -> Result<Option<u128>, ()> + 'a,
    ) -> impl Future<Output = Result<NativeObserver, Error>> + 'a {
        self.observe_native(launch, policy, Some(clock), choose)
    }
    fn observe_native<'a>(
        self,
        launch: Launch,
        policy: Policy,
        clock: Option<ClockPolicy>,
        mut choose: impl FnMut(&Catalog) -> Result<Option<u128>, ()> + 'a,
    ) -> impl Future<Output = Result<NativeObserver, Error>> + 'a {
        let cx = self.cx.clone();
        let budget = Budget::new(cx.clone(), policy);
        Attempt {
            cx,
            complete: false,
            inner: Box::pin(async move {
                Box::pin(bootstrap(self, launch, &budget?, &mut choose, clock)).await
            }),
        }
    }
}
// The guard lives OUTSIDE the async body: fence before dropping native work,
// including unpolled abandonment or unwinding from a local UI callback.
struct Attempt<F> {
    cx: Cx,
    complete: bool,
    inner: Pin<Box<F>>,
}
impl<T, F: Future<Output = Result<T, Error>>> Future for Attempt<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this.inner.as_mut().poll(task);
        if let Poll::Ready(value) = &result {
            this.complete = value.is_ok();
            if !this.complete {
                this.cx.cancel_fast(CancelKind::User);
            }
        }
        result
    }
}
impl<F> Drop for Attempt<F> {
    fn drop(&mut self) {
        if !self.complete {
            self.cx.cancel_fast(CancelKind::User);
        }
    }
}
#[allow(clippy::unnecessary_wraps)]
fn retain(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}

async fn choose_display(
    session: &mut ViewerSession,
    budget: &Budget,
    choose: &mut impl FnMut(&Catalog) -> Result<Option<u128>, ()>,
) -> Result<SelectedDisplay, Error> {
    let mut selection: DisplaySelection = session
        .select_display(budget.remaining()?)
        .map_err(Error::Display)?;
    let mut chosen = false;
    loop {
        budget.remaining()?;
        let (q, _) = session.io().map_err(Error::Session)?;
        selection.dispatch(q).map_err(Error::Display)?;
        if !chosen
            && let Some(catalog) = selection.catalog(q).map_err(Error::Display)?
            && let Some(handle) = choose(catalog).map_err(|()| Error::Application)?
        {
            budget.remaining()?;
            selection.choose(q, handle).map_err(Error::Display)?;
            chosen = true;
        }
        selection.transmit(q).map_err(Error::Display)?;
        if selection.is_complete() {
            return selection.finish(q).map_err(Error::Display);
        }
        session
            .drive(budget.wait()?, retain)
            .await
            .map_err(Error::Session)?;
    }
}

/// One bounded owned record. A second ready record remains in the transport's
/// existing bounded queue; it cannot overwrite the record being acted upon.
struct RecordSlot {
    bytes: Vec<u8>,
    len: usize,
    route: Route,
    kind: Kind,
}
impl RecordSlot {
    fn new(maximum: usize, route: Route, kind: Kind) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(maximum)
            .map_err(|_| Error::Allocation)?;
        bytes.resize(maximum, 0);
        Ok(Self {
            bytes,
            len: 0,
            route,
            kind,
        })
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        if route != self.route || bytes.get(6..8) != Some(&(self.kind as u16).to_be_bytes()) {
            return Ok(Disposition::Blocked);
        }
        if self.len != 0 {
            return Ok(Disposition::Blocked);
        }
        if bytes.len() > self.bytes.len() {
            return Err(());
        }
        self.bytes[..bytes.len()].copy_from_slice(bytes);
        self.len = bytes.len();
        Ok(Disposition::Consumed)
    }
}
async fn receive_record(
    session: &mut ViewerSession,
    budget: &Budget,
    slot: &mut RecordSlot,
) -> Result<(), Error> {
    while slot.len == 0 {
        session
            .drive(budget.wait()?, |r, b| slot.receive(r, b))
            .await
            .map_err(Error::Session)?;
    }
    budget.remaining()?;
    Ok(())
}
fn role_index(role: MediaRole, controlled: bool) -> Result<usize, Error> {
    match role {
        MediaRole::Configuration => Ok(0),
        MediaRole::Recovery => Ok(1),
        MediaRole::Video => Ok(2),
        MediaRole::Input if controlled => Ok(3),
        MediaRole::Input => Err(Error::Order),
    }
}
async fn attach_media(
    session: &mut ViewerSession,
    budget: &Budget,
    selected: &SelectedDisplay,
    controlled: bool,
) -> Result<(NegotiatedMedia, Route, Option<NegotiatedInput>), Error> {
    let mut channels: [Option<MediaChannel>; 4] = std::array::from_fn(|_| None);
    let mut slot = RecordSlot::new(
        attachment::BINDING_RECORD_BYTES,
        Route::Stream(session.routes.inbound),
        Kind::StreamBinding,
    )?;
    for _ in 0..if controlled { 4 } else { 3 } {
        receive_record(session, budget, &mut slot).await?;
        let Message::Binding(descriptor) = attachment::decode(
            &slot.bytes[..slot.len],
            session.opened.binding,
            session.opened.binding.id,
            &session.opened.selection.limits,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?
        else {
            return Err(Error::Order);
        };
        let index = role_index(descriptor.role, controlled)?;
        if channels[index].is_some() {
            return Err(Error::Order);
        }
        selected
            .check_binding(session.io().map_err(Error::Session)?.0, descriptor.binding)
            .map_err(Error::Display)?;
        let mut channel = session
            .accept_media_channel(&slot.bytes[..slot.len], budget.stage()?)
            .map_err(Error::Session)?;
        slot.bytes.fill(0);
        slot.len = 0;
        loop {
            let (q, _) = session.io().map_err(Error::Session)?;
            channel
                .dispatch(q, &budget.cx, || budget.live())
                .map_err(Error::Transport)?;
            channel
                .transmit(q, &budget.cx, || budget.live())
                .map_err(Error::Transport)?;
            if channel
                .finish(q, &budget.cx, || budget.live())
                .map_err(Error::Transport)?
                .is_some()
            {
                break;
            }
            session
                .drive(budget.wait()?, retain)
                .await
                .map_err(Error::Session)?;
        }
        channels[index] = Some(channel);
    }
    let [Some(configuration), Some(recovery), Some(video), input] = channels else {
        return Err(Error::Order);
    };
    let selection = session.opened.selection.clone();
    let (q, _) = session.io().map_err(Error::Session)?;
    let route = Route::Stream(
        configuration
            .completed_on(q)
            .map_err(Error::Transport)?
            .inbound,
    );
    let media = NegotiatedMedia::new(q, &selection, &configuration, &recovery, &video)
        .map_err(Error::Media)?;
    let input = input
        .map(|input| {
            NegotiatedInput::new(q, &selection, &configuration, input).map_err(Error::Input)
        })
        .transpose()?;
    if controlled != input.is_some() {
        return Err(Error::Order);
    }
    Ok((media, route, input))
}

/// A successful native result is held until the already-polled network turn
/// finishes. No select/race drops a healthy QUIC drive when the codec completes.
async fn during<T>(
    session: &mut ViewerSession,
    budget: &Budget,
    native: impl Future<Output = Result<T, decoder_startup::Error>>,
) -> Result<T, Error> {
    let mut native = pin!(native);
    loop {
        let mut result = None;
        let mut network = pin!(session.drive(budget.wait()?, retain));
        poll_fn(|task| {
            budget.remaining()?;
            if result.is_none()
                && let Poll::Ready(value) = native.as_mut().poll(task)
            {
                result = Some(value);
            }
            if let Some(Err(error)) = result.as_ref() {
                budget.cx.cancel_fast(CancelKind::User);
                return Poll::Ready(Err(Error::Decoder(*error)));
            }
            match network.as_mut().poll(task) {
                Poll::Ready(Err(error)) => {
                    budget.cx.cancel_fast(CancelKind::User);
                    Poll::Ready(Err(Error::Session(error)))
                }
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        })
        .await?;
        if let Some(value) = result {
            budget.remaining()?;
            return value.map_err(Error::Decoder);
        }
    }
}
async fn bootstrap(
    mut session: ViewerSession,
    launch: Launch,
    budget: &Budget,
    choose: &mut impl FnMut(&Catalog) -> Result<Option<u128>, ()>,
    clock: Option<ClockPolicy>,
) -> Result<NativeObserver, Error> {
    budget.remaining()?;
    if !native_control::profile(&session.opened.selection, clock.is_some()) {
        return Err(Error::InvalidConfiguration);
    }
    if let Some(clock) = clock {
        session.enable_clock_sync(clock).map_err(Error::Clock)?;
    }
    let selected = choose_display(&mut session, budget, choose).await?;
    let display = selected
        .display(session.io().map_err(Error::Session)?.0)
        .map_err(Error::Display)?;
    let (media, route, input) =
        attach_media(&mut session, budget, &selected, clock.is_some()).await?;
    let mut config = RecordSlot::new(
        session.opened.selection.limits.max_control_message_bytes() as usize,
        route,
        Kind::DecoderConfiguration,
    )?;
    receive_record(&mut session, budget, &mut config).await?;
    let (q, _) = session.io().map_err(Error::Session)?;
    let setup = selected
        .decoder_setup(q, &media, budget.stage()?)
        .map_err(Error::Display)?;
    let receive = media
        .receiver_config(q, ReceivePolicy::default())
        .map_err(Error::Media)?;
    let prepared = decoder_startup::Viewer::prepare(
        budget.cx.clone(),
        q,
        setup,
        &config.bytes[..config.len],
        receive,
    )
    .map_err(Error::Decoder)?;
    drop(config);
    let mut decoder = Box::pin(during(&mut session, budget, prepared.configure(launch))).await?;
    decoder
        .check_transport(session.io().map_err(Error::Session)?.0)
        .map_err(Error::Decoder)?;
    send_decoder(&mut session, budget, &mut decoder).await?;
    let initial: PresentationReceipt = loop {
        let mut failure = None;
        media
            .receive_ready(
                &budget.cx,
                session.io().map_err(Error::Session)?.0,
                || budget.live(),
                |channel, bytes| {
                    decoder.receive_media(channel, bytes).map_err(|e| {
                        failure = Some(e);
                    })?;
                    Ok(Disposition::Consumed)
                },
            )
            .map_err(|e| failure.map_or(Error::Media(e), Error::Decoder))?;
        let completion = during(&mut session, budget, decoder.present_first()).await?;
        decoder
            .check_transport(session.io().map_err(Error::Session)?.0)
            .map_err(Error::Decoder)?;
        if let Some(receipt) = completion {
            break receipt;
        }
    };
    send_decoder(&mut session, budget, &mut decoder).await?;
    selected
        .check(session.io().map_err(Error::Session)?.0)
        .map_err(Error::Display)?;
    budget.remaining()?;
    let parent = session.opened.binding;
    let view = media.binding();
    let presentation = streaming::Presentation {
        frame: initial.frame,
        stage: initial.stage,
    };
    let viewer = session
        .into_streaming_presented(media, decoder, initial)
        .map_err(Error::Streaming)?;
    Ok(NativeObserver {
        selected,
        display,
        initial: presentation,
        viewer: Box::new(viewer),
        cx: budget.cx.clone(),
        input,
        parent,
        view,
    })
}
async fn send_decoder(
    session: &mut ViewerSession,
    budget: &Budget,
    decoder: &mut decoder_startup::Viewer,
) -> Result<(), Error> {
    loop {
        budget.remaining()?;
        let queued = decoder
            .transmit(session.io().map_err(Error::Session)?.0)
            .map_err(Error::Decoder)?;
        session
            .drive(budget.wait()?, retain)
            .await
            .map_err(Error::Session)?;
        decoder
            .check_transport(session.io().map_err(Error::Session)?.0)
            .map_err(Error::Decoder)?;
        if queued {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests;
