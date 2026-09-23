//! Persistent, capacity-one acceptance on the canonical native socket path.
//! Each arrival receives new local IDs and its own cancellation context. A
//! completed/failed session is never resumed or replayed; the original ingress
//! restriction and local identity remain requirements across every rebind.
use super::{Error as HostError, Host, IngressCheck, Request, Server};
use asupersync::{
    cx::Cx,
    runtime::RuntimeHandle,
    time::{TimerDriverHandle, TimerHandle},
    types::{CancelKind, Time},
};
use fr_transport::native_accept::{self, Listener};
use std::{
    future::{Future, poll_fn},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Runtime,
    Clock,
    Cancelled,
    Callback,
    Host(HostError),
    RetainedTransport,
    IdentityReuse,
    Exhausted,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native-listener: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Bounds admission turnover, including a flood of promptly refused clients.
/// Established-session length is controlled by its existing authority owners.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub cooldown: Duration,
    pub retirement_timeout: Duration,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            cooldown: Duration::from_millis(250),
            retirement_timeout: Duration::from_secs(2),
        }
    }
}
impl Policy {
    fn validate(self) -> Result<(), Error> {
        if self.cooldown < Duration::from_millis(100)
            || self.cooldown > Duration::from_secs(5)
            || self.retirement_timeout < Duration::from_millis(1)
            || self.retirement_timeout > Duration::from_secs(5)
        {
            return Err(Error::Configuration);
        }
        Ok(())
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    pub attempts: u64,
    pub admitted: u64,
    pub refused: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Continue,
    Stop,
}

/// Local integration, never invoked under transport or authority locks. All
/// callbacks must poll/return without blocking. `request` supplies independently
/// allocated, fresh unpredictable connection and remote-session IDs on EVERY
/// attempt (including after an idle timeout); it must never recycle an old grant.
/// Boot and selected OS-session identities cannot change within this listener.
/// The adjacent-ID guard catches accidental immediate reuse, not all historical
/// collisions; the allocator remains responsible for global freshness.
///
/// `serve` must stay alive for the whole peer session, for example by awaiting
/// the existing native desktop Incoming service. An arbitrary output is retained
/// exactly and delivered once to `completed`; it is never interpreted as effect
/// rollback or permission to replay. It may release a retained original transport.
/// Rebinding remains forbidden until transport retirement is observed.
/// The independent shared desktop and its native cleanup stay with their owner.
pub trait Application {
    type Output;
    fn request(&mut self, attempt: u64) -> Result<Request, Error>;
    fn serve(&mut self, host: Host) -> impl Future<Output = Self::Output>;
    fn completed(
        &mut self,
        statistics: Statistics,
        outcome: Result<Self::Output, HostError>,
    ) -> Result<Action, Error>;
}

impl Server {
    /// Explicit capacity-one persistent hosting. This is NOT simultaneous
    /// multi-client QUIC routing: one admitted peer owns this destination until
    /// its original transport is gone. Other arrivals are not queued or migrated.
    ///
    /// As with `run_on_protected_listener`, the boundary must ALREADY be enforced
    /// for this exact socket; the callback checks continuing local evidence only.
    /// Native TLS, exact-endpoint `LocalAPI` authorization and Host startup use the
    /// existing canonical implementation on every attempt, without a fallback.
    ///
    /// `supervisor` must be dedicated to this service, separate from the broker,
    /// source and credential service. `RuntimeHandle` supplies fresh peer contexts;
    /// no task is spawned. The original listener is consumed at call time, but
    /// admission starts on the first poll. Idle/refused attempts do not restart
    /// their own deadline. Every replacement follows a cooldown and observed
    /// transport retirement. Stalled cleanup stops the service, never overlaps it.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn serve_serial_on_protected_listener<'a, A: Application + 'a>(
        &'a mut self,
        supervisor: Cx,
        runtime: RuntimeHandle,
        listener: Listener,
        policy: Policy,
        ingress: IngressCheck,
        application: &'a mut A,
    ) -> impl Future<Output = Result<Statistics, Error>> + 'a {
        let address = listener.local_addr();
        let transport = listener.configuration();
        let clock = supervisor.clone();
        let identity = self.identity.clone();
        let boundary = ingress;
        let check: Check = Arc::new(move || {
            clock.checkpoint().map_err(|_| Error::Cancelled)?;
            if !boundary(address) {
                return Err(Error::Host(HostError::IngressUnavailable));
            }
            identity
                .status(&clock)
                .map_err(|e| Error::Host(HostError::Tailnet(e)))?;
            Ok(())
        });
        let inside = check.clone();
        let cx = supervisor.clone();
        Guard {
            cx: supervisor,
            check,
            inner: Some(Box::pin(async move {
                policy.validate()?;
                let mut stats = Statistics::default();
                let mut listener = Some(listener);
                let mut previous = None;
                loop {
                    inside()?;
                    stats.attempts = stats.attempts.checked_add(1).ok_or(Error::Exhausted)?;
                    let request = application.request(stats.attempts)?;
                    request
                        .session
                        .check_incoming()
                        .map_err(|e| Error::Host(HostError::Session(e)))?;
                    let ids = (
                        request.session.binding.host_boot,
                        request.session.binding.os_session,
                        request.session.binding.remote_session,
                        request.connection_id,
                    );
                    if previous.is_some_and(|old: (_, _, _, _)| {
                        old.0 != ids.0 || old.1 != ids.1 || old.2 == ids.2 || old.3 == ids.3
                    }) {
                        return Err(Error::IdentityReuse);
                    }
                    previous = Some(ids);
                    inside()?;
                    let peer = runtime
                        .try_request_cx_with_budget(cx.budget())
                        .map_err(|_| Error::Runtime)?;
                    // Native accept and every application poll inherit this gate.
                    // The marker remains in the original transport after handoff.
                    let marker = Arc::new(());
                    let retained = marker.clone();
                    let gate = inside.clone();
                    let fence = super::PanicFence(&peer, false);
                    let socket = match listener.take() {
                        Some(socket) => socket,
                        None => Listener::bind(&peer, address, transport)
                            .await
                            .map_err(|e| Error::Host(HostError::Accept(e)))?,
                    };
                    inside()?;
                    let outcome = {
                        let running = self.run_on_protected_listener(
                            &peer,
                            socket,
                            request,
                            Arc::new(move |bound| {
                                // The check itself retains the transport-retirement marker.
                                Arc::strong_count(&retained) > 1
                                    && bound == address
                                    && gate().is_ok()
                            }),
                            |host| application.serve(host),
                        );
                        let mut running = std::pin::pin!(running);
                        poll_fn(|task| {
                            // Native Sleep binds at poll time. Use the SAME peer
                            // context, never an unrelated ambient timer domain.
                            let _current = Cx::set_current(Some(peer.clone()));
                            running.as_mut().poll(task)
                        })
                        .await
                    }; // Destroy the scoped native future before inspecting its lease.
                    drop(fence);
                    // Publish the authentic outcome before awaiting cleanup. The
                    // callback may collect effects or release a retained original
                    // transport; stalled cleanup must not erase its receipt.
                    let failure = outcome.as_ref().err().copied();
                    if failure.is_some() {
                        stats.refused += 1;
                    } else {
                        stats.admitted += 1;
                    }
                    let action = application.completed(stats, outcome)?;
                    inside()?;
                    // A handed-off Host/HostSession can remain on a sibling hub.
                    // Wait for its real transport destruction, not merely cancel.
                    let until = after(&cx, policy.retirement_timeout)?;
                    wait(&cx, until, || Arc::strong_count(&marker) == 1).await?;
                    if Arc::strong_count(&marker) != 1 {
                        return Err(Error::RetainedTransport);
                    }
                    if let Some(error) = failure
                        && !peer_refusal(error)
                    {
                        return Err(Error::Host(error));
                    }
                    if action == Action::Stop {
                        return Ok(stats);
                    }
                    let until = after(&cx, policy.cooldown)?;
                    wait(&cx, until, || false).await?;
                }
            })),
            timer: None,
            previous: None,
        }
    }
}
// Only unambiguous peer-local failures are recoverable. A changing host identity,
// failed credential service, unavailable LocalAPI or loss of ingress ends hosting.
fn peer_refusal(error: HostError) -> bool {
    matches!(
        error,
        HostError::Accept(
            native_accept::Error::InitialTimeout
                | native_accept::Error::InitialBudget
                | native_accept::Error::HandshakeTimeout
                | native_accept::Error::Handshake
                | native_accept::Error::PeerChanged
        ) | HostError::Tailnet(
            fr_tailnet::Error::ScopeDenied
                | fr_tailnet::Error::SharedPeer
                | fr_tailnet::Error::MachineNotAuthorized
                | fr_tailnet::Error::TailnetMembershipUnverifiable
        )
    )
}
fn now(cx: &Cx) -> Result<Time, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    Ok(cx.timer_driver().ok_or(Error::Clock)?.now())
}
fn after(cx: &Cx, duration: Duration) -> Result<Time, Error> {
    now(cx)?
        .as_nanos()
        .checked_add(u64::try_from(duration.as_nanos()).map_err(|_| Error::Clock)?)
        .map(Time::from_nanos)
        .ok_or(Error::Clock)
}
async fn wait(cx: &Cx, until: Time, ready: impl Fn() -> bool) -> Result<(), Error> {
    let driver = cx.timer_driver().ok_or(Error::Clock)?;
    let mut timer = Timer {
        driver,
        handle: None,
    };
    poll_fn(|task| {
        let now = now(cx)?;
        if ready() || now >= until {
            return Poll::Ready(Ok(()));
        }
        timer.arm(now.saturating_add_nanos(10_000_000).min(until), task);
        Poll::Pending
    })
    .await
}
type Check = Arc<dyn Fn() -> Result<(), Error> + Send + Sync>;
struct Timer {
    driver: TimerDriverHandle,
    handle: Option<TimerHandle>,
}
impl Timer {
    fn arm(&mut self, at: Time, task: &Context<'_>) {
        if let Some(handle) = self.handle.take() {
            let _ = self.driver.cancel(&handle);
        }
        self.handle = Some(self.driver.register(at, task.waker().clone()));
        if self.driver.now() >= at {
            task.waker().wake_by_ref();
        }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.driver.cancel(&handle);
        }
    }
}
// Outer ownership makes expiry/revocation/panic terminal even when the caller
// retains a failed future. Fence before dropping its active native work.
struct Guard<F> {
    cx: Cx,
    check: Check,
    inner: Option<Pin<Box<F>>>,
    timer: Option<Timer>,
    previous: Option<Time>,
}
impl<F> Guard<F> {
    fn finish(&mut self) {
        self.cx.cancel_fast(CancelKind::User);
        drop(self.inner.take());
        self.timer = None;
    }
}
impl<F: Future<Output = Result<Statistics, Error>>> Future for Guard<F> {
    type Output = Result<Statistics, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let mut turn = Turn {
            owner: self.get_mut(),
            complete: false,
        };
        let this = &mut *turn.owner;
        let result = (|| {
            (this.check)()?;
            let now = now(&this.cx)?;
            if this.previous.is_some_and(|old| now < old) {
                return Poll::Ready(Err(Error::Clock));
            }
            this.previous = Some(now);
            let result = this
                .inner
                .as_mut()
                .ok_or(Error::Cancelled)?
                .as_mut()
                .poll(task);
            if result.is_pending() {
                (this.check)()?;
                let driver = this.cx.timer_driver().ok_or(Error::Clock)?;
                let timer = this.timer.get_or_insert_with(|| Timer {
                    driver,
                    handle: None,
                });
                timer.arm(now.saturating_add_nanos(10_000_000), task);
            }
            result
        })();
        if result.is_ready() {
            turn.owner.finish();
        }
        turn.complete = true;
        result
    }
}
struct Turn<'a, F> {
    owner: &'a mut Guard<F>,
    complete: bool,
}
impl<F> Drop for Turn<'_, F> {
    fn drop(&mut self) {
        if !self.complete {
            self.owner.finish();
        }
    }
}
impl<F> Drop for Guard<F> {
    fn drop(&mut self) {
        self.finish();
    }
}
