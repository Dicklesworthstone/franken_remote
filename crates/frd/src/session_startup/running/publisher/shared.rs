//! Normal display selection and role attachments for an independently owned source.
//! Reuse the existing canonical host driver; never make a per-viewer capture worker.
use super::{
    Attempt, Budget, CatalogService, Duration, Error, Future, HostSession, MediaRole,
    NegotiatedMedia, ObservationControl, Policy, SelectedDisplay, SendPolicy, attach, block,
    decoder_startup, drive, native_control,
};
use crate::media::{
    SharedCaptureUpdate,
    shared_publisher::{JoinQueue, Publisher},
};
use crate::session_startup::running::SharedHost;

impl HostSession {
    /// Start the first observer of a locally selected, independently authorized
    /// source. `initial` is the source's actual shared IDR; Publisher validates its
    /// source, pool and frame identity before admitting it. Local source consent
    /// must continue independently of this viewer. No OS permission is created.
    ///
    /// This borrows only a pristine publisher during selection/attachment. Once
    /// admitted, drive the returned `SharedHost` and `Publisher` on sibling tasks.
    /// It returns with the existing decoder handshake pending, not with a claim
    /// of readiness or visibility. The caller owns capture and eventual reaping.
    pub fn start_shared_display<'a>(
        self,
        publisher: &'a mut Publisher,
        initial: &'a SharedCaptureUpdate,
        timeout: Duration,
        send: SendPolicy,
        entropy: impl FnMut() -> Result<u128, ()> + 'a,
    ) -> impl Future<Output = Result<SharedHost, Error>> + 'a {
        self.start_shared_display_capped(publisher, initial, timeout, u64::MAX, send, entropy)
    }
    // Keep the first Host's original deadline across approval and display startup.
    // Passing an absolute bound avoids renewing it through scheduling delays.
    pub(crate) fn start_shared_display_capped<'a>(
        self,
        publisher: &'a mut Publisher,
        initial: &'a SharedCaptureUpdate,
        timeout: Duration,
        until: u64,
        send: SendPolicy,
        mut entropy: impl FnMut() -> Result<u128, ()> + 'a,
    ) -> impl Future<Output = Result<SharedHost, Error>> + 'a {
        let control = self.opened.control.clone();
        let queue = publisher.join_queue();
        let budget = source_budget(control.clone(), queue.clone(), timeout).and_then(|mut b| {
            b.until = b.until.min(until);
            b.remaining()?;
            Ok(b)
        });
        Attempt {
            control,
            complete: false,
            inner: Box::pin(async move {
                let budget = budget?;
                let configuration = publisher.initial_configuration().map_err(Error::Shared)?;
                let (mut host, selected, media) =
                    Box::pin(select_media(self, &queue, &budget, &mut entropy)).await?;
                budget.remaining()?;
                let q = host.io().map_err(Error::Session)?.0;
                let setup = selected
                    .decoder_setup(q, &media, budget.stage()?)
                    .map_err(Error::Display)?
                    .capped_at(budget.until);
                let startup = decoder_startup::Host::new_shared(
                    budget.control.clone(),
                    q,
                    setup,
                    configuration,
                    initial.clone(),
                )
                .map_err(Error::Startup)?;
                let sender = media
                    .sender(q, budget.control.clone(), send)
                    .map_err(Error::Routes)?;
                let subscriber = publisher
                    .admit_pending(startup, sender, media, q)
                    .map_err(Error::Shared)?;
                let mut shared = host.into_shared(subscriber).map_err(Error::Session)?;
                shared
                    .retain_selected_display(selected, budget.until)
                    .map_err(Error::Session)?;
                budget.remaining()?;
                Ok(shared)
            }),
        }
    }
    /// Join the same selected source without borrowing or stalling its capture
    /// task. Only that display is offered; explicit choice and three fresh
    /// one-use attachments run on this already-approved session. The original
    /// absolute deadline covers selection, attachment, waiting for a rate-admitted
    /// IDR, configuration and first decode. No old picture is replayed as fresh.
    pub fn join_shared_display<'a>(
        self,
        queue: JoinQueue,
        timeout: Duration,
        send: SendPolicy,
        entropy: impl FnMut() -> Result<u128, ()> + 'a,
    ) -> impl Future<Output = Result<SharedHost, Error>> + 'a {
        self.join_shared_display_capped(queue, timeout, u64::MAX, send, entropy)
    }
    // Incoming Host negotiation and local approval have already spent part of
    // this absolute budget. Do not turn its remainder into a fresh relative
    // deadline: even scheduling time between calls must count against it.
    pub(crate) fn join_shared_display_capped<'a>(
        self,
        queue: JoinQueue,
        timeout: Duration,
        until: u64,
        send: SendPolicy,
        mut entropy: impl FnMut() -> Result<u128, ()> + 'a,
    ) -> impl Future<Output = Result<SharedHost, Error>> + 'a {
        let control = self.opened.control.clone();
        let budget = source_budget(control.clone(), queue.clone(), timeout).and_then(|mut b| {
            b.until = b.until.min(until);
            b.remaining()?;
            Ok(b)
        });
        Attempt {
            control,
            complete: false,
            inner: Box::pin(async move {
                let budget = budget?;
                let (host, selected, media) =
                    Box::pin(select_media(self, &queue, &budget, &mut entropy)).await?;
                budget.remaining()?;
                let mut shared = host
                    .join_shared(&queue, media, send, budget.stage()?)
                    .map_err(Error::Session)?;
                shared
                    .retain_selected_display(selected, budget.until)
                    .map_err(Error::Session)?;
                budget.remaining()?;
                Ok(shared)
            }),
        }
    }
}
fn source_budget(
    control: ObservationControl,
    source: JoinQueue,
    timeout: Duration,
) -> Result<Budget, Error> {
    if timeout > Duration::from_secs(2) {
        return Err(Error::InvalidConfiguration);
    }
    let mut budget = Budget::new(
        control,
        Policy {
            timeout,
            ..Policy::default()
        },
    )?;
    budget.source = Some(source);
    budget.remaining()?;
    Ok(budget)
}
async fn select_media(
    mut host: HostSession,
    queue: &JoinQueue,
    budget: &Budget,
    entropy: &mut impl FnMut() -> Result<u128, ()>,
) -> Result<(HostSession, SelectedDisplay, NegotiatedMedia), Error> {
    budget.remaining()?;
    if !native_control::profile(host.selection(), false) {
        return Err(Error::InvalidConfiguration);
    }
    while !host.sharing_surface_ready().map_err(Error::Session)? {
        drive(&mut host, budget, entropy, &mut block).await?;
    }
    let catalog = queue.selected_catalog().map_err(Error::Shared)?;
    let mut choice = host
        .select_display(catalog, budget.remaining()?)
        .map_err(Error::Display)?;
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
        .finish(host.io().map_err(Error::Session)?.0)
        .map_err(Error::Display)?;
    let configuration = attach(
        &mut host,
        &mut selected,
        MediaRole::Configuration,
        budget,
        entropy,
    )
    .await?;
    let recovery = attach(
        &mut host,
        &mut selected,
        MediaRole::Recovery,
        budget,
        entropy,
    )
    .await?;
    let video = attach(&mut host, &mut selected, MediaRole::Video, budget, entropy).await?;
    budget.remaining()?;
    let selection = host.selection().clone();
    let q = host.io().map_err(Error::Session)?.0;
    selected
        .revalidate(q, &queue.selected_catalog().map_err(Error::Shared)?)
        .map_err(Error::Display)?;
    let media = NegotiatedMedia::new(q, &selection, &configuration, &recovery, &video)
        .map_err(Error::Routes)?;
    Ok((host, selected, media))
}
