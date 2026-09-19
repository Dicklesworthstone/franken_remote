//! Bounded observation reconnection on the canonical installed-tailnet client.
//! No input owner, old action, decoder reference, approval or lease crosses attempts.
mod native;
pub use native::{native_control_view, native_control_view_with_setup, native_view};

use super::{Client, Configuration, Error as ConnectionError};
use crate::session_startup::{ObserverError, StreamingViewerError, Viewer};
use crate::worker::Deadline;
use asupersync::{
    cx::Cx,
    runtime::RuntimeHandle,
    time::sleep_until,
    types::{Budget, CancelKind, Time},
};
use fr_tailnet::PeerSelector;
use fr_wire::negotiation::{Offer, Role};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Includes the initial attempt. Success/traffic does not reset the counter.
    pub max_attempts: u8,
    pub initial_backoff: Duration,
    pub maximum_backoff: Duration,
    /// Independent cleanup budget, including time before its first poll.
    pub cleanup_timeout: Duration,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(250),
            maximum_backoff: Duration::from_secs(4),
            cleanup_timeout: Duration::from_secs(2),
        }
    }
}
impl Policy {
    pub fn validate(self) -> Result<(), Failure> {
        if !(1..=32).contains(&self.max_attempts)
            || self.initial_backoff < Duration::from_millis(10)
            || self.maximum_backoff < self.initial_backoff
            || self.maximum_backoff > Duration::from_secs(30)
            || self.cleanup_timeout < Duration::from_millis(1)
            || self.cleanup_timeout > Duration::from_secs(5)
        {
            return Err(Failure::InvalidPolicy);
        }
        Ok(())
    }
    fn delay(self, attempt: u8) -> Duration {
        self.initial_backoff
            .checked_mul(1_u32 << (attempt - 1))
            .unwrap_or(self.maximum_backoff)
            .min(self.maximum_backoff)
    }
}

/// Errors deliberately contain no destination, input, screen or credential data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    InvalidPolicy,
    ObservationOnly,
    ControlCapableOnly,
    MissingRuntime,
    Clock,
    Cancelled,
    Notification,
    Connection(ConnectionError),
    Observation(ObserverError),
    Cleanup,
    CleanupExpired,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Failure {}

/// Bounded metadata for UI and structured logs, not proof of visible pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Connecting {
        attempt: u8,
    },
    Authenticated {
        attempt: u8,
    },
    Cleaning {
        attempt: u8,
        failure: Option<Failure>,
    },
    Reconnecting {
        attempt: u8,
        delay: Duration,
        failure: Failure,
    },
    Completed {
        attempt: u8,
    },
    Stopped {
        attempt: u8,
        failure: Failure,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackError;
impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("application callback failed")
    }
}
impl std::error::Error for CallbackError {}

/// The application owns its native observer until cleanup finishes. `run` is
/// called afresh only after authentic TLS admission; it must not replay retained
/// input. `cleanup` is mandatory even if discovery failed before `run` started.
/// Store a started `NativeObserver` in this owner and call its `reap_media` there.
/// Return Err if cleanup cannot prove the old worker is gone: no retry follows.
/// Status callbacks must be bounded/nonblocking; Err stops the entire operation.
pub trait Application {
    type Output;
    fn run(
        &mut self,
        attempt: u8,
        viewer: Viewer,
    ) -> impl Future<Output = Result<Self::Output, ObserverError>>;
    fn cleanup(
        &mut self,
        cx: &Cx,
        deadline: Deadline,
    ) -> impl Future<Output = Result<(), CallbackError>>;
    fn status(&mut self, status: Status) -> Result<(), CallbackError>;
}

impl Client {
    /// Recover observation after a transient network failure, with a fresh
    /// session context, authenticated connection and full startup on each try.
    /// The dedicated supervisor Cx is cancelled on every final exit or drop;
    /// never pass the daemon-wide context. `RuntimeHandle` supplies independent
    /// request contexts so closing one attempt cannot cancel its replacement.
    ///
    /// Only `Role::Observe` is accepted. A previously controlling UI may use this
    /// path to recover viewing, but input reacquisition remains a separate,
    /// explicit user operation with new consent, mapping and freshness evidence.
    /// The first verified destination snapshot pins identity: address/name/key,
    /// daemon or membership changes stop rather than silently retargeting.
    /// Cleanup runs before backoff or another discovery, under its own bounded
    /// context even when the session or supervisor has been cancelled.
    // Keep one linear attempt/cleanup/wait ownership sequence visible.
    #[allow(clippy::too_many_arguments)]
    pub fn run_observing<'a, A: Application + 'a>(
        &'a mut self,
        cx: Cx,
        runtime: RuntimeHandle,
        selector: PeerSelector<'a>,
        cfg: Configuration,
        offer: Offer,
        policy: Policy,
        application: &'a mut A,
    ) -> impl Future<Output = Result<A::Output, Failure>> + 'a {
        self.run_reconnecting(
            cx,
            runtime,
            selector,
            cfg,
            offer,
            policy,
            Role::Observe,
            Failure::ObservationOnly,
            application,
        )
    }

    /// Reconnect a control-capable *viewing* session without carrying control
    /// authority across attempts. Every retry performs fresh destination
    /// validation, TLS/startup, display selection, decoder setup and cleanup.
    /// The application receives a new `Viewer` and may expose an explicit local
    /// Take Control action only after the new session has fresh mapping and
    /// presentation evidence. This supervisor never replays a request, lease,
    /// ticket, action, held state, approval, or old media/reference identity.
    #[allow(clippy::too_many_arguments)]
    pub fn run_control_capable<'a, A: Application + 'a>(
        &'a mut self,
        cx: Cx,
        runtime: RuntimeHandle,
        selector: PeerSelector<'a>,
        cfg: Configuration,
        offer: Offer,
        policy: Policy,
        application: &'a mut A,
    ) -> impl Future<Output = Result<A::Output, Failure>> + 'a {
        self.run_reconnecting(
            cx,
            runtime,
            selector,
            cfg,
            offer,
            policy,
            Role::RequestControl,
            Failure::ControlCapableOnly,
            application,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn run_reconnecting<'a, A: Application + 'a>(
        &'a mut self,
        cx: Cx,
        runtime: RuntimeHandle,
        selector: PeerSelector<'a>,
        cfg: Configuration,
        offer: Offer,
        policy: Policy,
        expected_role: Role,
        role_failure: Failure,
        application: &'a mut A,
    ) -> impl Future<Output = Result<A::Output, Failure>> + 'a {
        Owned {
            cx: cx.clone(),
            inner: Box::pin(async move {
                policy.validate()?;
                cfg.validate().map_err(Failure::Connection)?;
                offer
                    .validate()
                    .map_err(|_| Failure::Connection(ConnectionError::InvalidConfiguration))?;
                if offer.role != expected_role {
                    return Err(role_failure);
                }
                let mut identity = None;
                let mut clock = Clock::new(&cx)?;
                for attempt in 1..=policy.max_attempts {
                    clock.live(&cx)?;
                    notify(application, Status::Connecting { attempt })?;
                    clock.live(&cx)?;
                    let session = runtime
                        .try_request_cx_with_budget(cx.budget())
                        .map_err(|_| Failure::MissingRuntime)?;
                    let outcome = guarded(
                        &cx,
                        &mut clock,
                        Owned {
                            cx: session.clone(),
                            inner: Box::pin(async {
                                if identity.is_none() {
                                    identity = Some(
                                        self.api.peer_target(&session, selector).await.map_err(
                                            |e| Failure::Connection(ConnectionError::Tailnet(e)),
                                        )?,
                                    );
                                }
                                self.run_checked(
                                    session.clone(),
                                    selector,
                                    cfg,
                                    offer.clone(),
                                    identity.as_ref(),
                                    |viewer| async {
                                        notify(application, Status::Authenticated { attempt })?;
                                        cx.checkpoint().map_err(|_| Failure::Cancelled)?;
                                        session.checkpoint().map_err(|_| Failure::Cancelled)?;
                                        application
                                            .run(attempt, viewer)
                                            .await
                                            .map_err(Failure::Observation)
                                    },
                                )
                                .await
                                .map_err(Failure::Connection)?
                            }),
                        },
                    )
                    .await;
                    // Owned cancels before dropping the application future. All
                    // external native storage now belongs only to cleanup.
                    let failure = outcome.as_ref().err().copied();
                    let notice = notify(application, Status::Cleaning { attempt, failure });
                    let clean = runtime
                        .try_request_cx_with_budget(Budget::INFINITE)
                        .map_err(|_| Failure::MissingRuntime)?;
                    let result = cleanup(clean, policy.cleanup_timeout, application).await;
                    if let Err(error) = result {
                        let _ = notify(
                            application,
                            Status::Stopped {
                                attempt,
                                failure: error,
                            },
                        );
                        return Err(error);
                    }
                    notice?;
                    if let Err(error) = clock.live(&cx) {
                        let _ = notify(
                            application,
                            Status::Stopped {
                                attempt,
                                failure: error,
                            },
                        );
                        return Err(error);
                    }
                    match outcome {
                        Ok(value) => {
                            notify(application, Status::Completed { attempt })?;
                            clock.live(&cx)?;
                            return Ok(value);
                        }
                        Err(error) if retryable(error) && attempt < policy.max_attempts => {
                            let delay = policy.delay(attempt);
                            // Notification time counts against this fixed wait.
                            let until = after(clock.live(&cx)?, delay)?;
                            notify(
                                application,
                                Status::Reconnecting {
                                    attempt: attempt + 1,
                                    delay,
                                    failure: error,
                                },
                            )?;
                            if let Err(failure) = guarded(&cx, &mut clock, async {
                                sleep_until(until).await;
                                Ok(())
                            })
                            .await
                            {
                                let _ = notify(application, Status::Stopped { attempt, failure });
                                return Err(failure);
                            }
                        }
                        Err(error) => {
                            notify(
                                application,
                                Status::Stopped {
                                    attempt,
                                    failure: error,
                                },
                            )?;
                            return Err(error);
                        }
                    }
                }
                unreachable!("validated nonzero finite attempt count")
            }),
        }
    }
}
fn notify(app: &mut impl Application, status: Status) -> Result<(), Failure> {
    app.status(status).map_err(|_| Failure::Notification)
}

/// Retry explicit transient transport/availability outcomes and exhausted media
/// delivery horizons during streaming. The latter restart the entire observation
/// through authenticated startup and a fresh decoder, ONLY after native cleanup;
/// they never resume an invalid reference chain or restore an old input lease.
/// Established control and telemetry can wrap the same transport failure; their
/// typed transport errors share the policy, not their input/authority failures.
/// Closed, cancelled, unauthorized, malformed, handler and actual codec failures
/// remain terminal. A generic closure is not guessed to be network or frame loss.
/// A reference-expiry report which cannot obtain three fresh media roles may
/// restart observation AFTER cleanup. The old namespace/decoder is never reused,
/// and decode failure does not become retryable merely because roles ran out.
pub fn retryable(failure: Failure) -> bool {
    use crate::session_startup::ControlledViewerError;

    match failure {
        Failure::Connection(ConnectionError::Tailnet(
            fr_tailnet::Error::LocalApiUnavailable
            | fr_tailnet::Error::BackendNotRunning
            | fr_tailnet::Error::Timeout,
        ))
        | Failure::Observation(ObserverError::Streaming(StreamingViewerError::Recovery(
            crate::media_quic::recovery::Error::NamespaceExhausted(
                fr_wire::recovery_request::Reason::ReferenceExpired
                | fr_wire::recovery_request::Reason::RecoveryExpired,
            ),
        ))) => true,
        Failure::Observation(ObserverError::Streaming(
            StreamingViewerError::Transport(e)
            | StreamingViewerError::Recovery(crate::media_quic::recovery::Error::Transport(e))
            | StreamingViewerError::Replacement(crate::media_quic::replacement::Error::Transport(e))
            | StreamingViewerError::Routes(crate::media_quic::Error::Transport(e))
            | StreamingViewerError::Feedback(crate::media::ReceiverFeedbackError::Transport(e))
            | StreamingViewerError::PresentedState(crate::media::PresentedStateError::Transport(e))
            | StreamingViewerError::Control(
                ControlledViewerError::Media(crate::media_quic::Error::Transport(e))
                | ControlledViewerError::Input(crate::input_quic::Error::Transport(e)),
            ),
        )) => transport_retryable(e),
        Failure::Observation(ObserverError::Streaming(
            StreamingViewerError::Session(e)
            | StreamingViewerError::Control(ControlledViewerError::Session(e)),
        )) => session_retryable(e),
        Failure::Observation(ObserverError::Streaming(
            StreamingViewerError::Delivery(e)
            | StreamingViewerError::Media(crate::media::Error::Receiver(e)),
        )) => matches!(
            e,
            fr_media::delivery::DeliveryError::ReferenceExpired
                | fr_media::delivery::DeliveryError::RecoveryExpired
        ),
        _ => false,
    }
}
fn session_retryable(error: crate::session_startup::Error) -> bool {
    match error {
        crate::session_startup::Error::Transport(e) => transport_retryable(e),
        crate::session_startup::Error::Expired
        | crate::session_startup::Error::ClientRenewal(fr_client::authority::Error::Expired) => {
            true
        }
        _ => false,
    }
}
fn transport_retryable(error: fr_transport::quic::Error) -> bool {
    matches!(
        error,
        fr_transport::quic::Error::Native | fr_transport::quic::Error::Expired
    )
}

// Guard construction, not first polling, establishes cancellation ownership.
// Drop fences before the pinned inner future or its native work is abandoned.
struct Owned<F> {
    cx: Cx,
    inner: Pin<Box<F>>,
}
impl<F: Future> Future for Owned<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this.inner.as_mut().poll(task);
        if result.is_ready() {
            this.cx.cancel_fast(CancelKind::User);
        }
        result
    }
}
impl<F> Drop for Owned<F> {
    fn drop(&mut self) {
        self.cx.cancel_fast(CancelKind::User);
    }
}
struct Clock(Time);
impl Clock {
    fn new(cx: &Cx) -> Result<Self, Failure> {
        Ok(Self(
            cx.timer_driver().ok_or(Failure::MissingRuntime)?.now(),
        ))
    }
    fn read(&mut self, cx: &Cx) -> Result<Time, Failure> {
        let at = cx.timer_driver().ok_or(Failure::MissingRuntime)?.now();
        if at < self.0 {
            return Err(Failure::Clock);
        }
        self.0 = at;
        Ok(at)
    }
    fn live(&mut self, cx: &Cx) -> Result<Time, Failure> {
        cx.checkpoint().map_err(|_| Failure::Cancelled)?;
        self.read(cx)
    }
}
fn after(at: Time, duration: Duration) -> Result<Time, Failure> {
    Ok(Time::from_nanos(
        at.as_nanos()
            .checked_add(u64::try_from(duration.as_nanos()).map_err(|_| Failure::Clock)?)
            .ok_or(Failure::Clock)?,
    ))
}
const WAKE: Duration = Duration::from_millis(10);
async fn guarded<T>(
    cx: &Cx,
    clock: &mut Clock,
    future: impl Future<Output = Result<T, Failure>>,
) -> Result<T, Failure> {
    let mut future = pin!(future);
    let mut timer = pin!(sleep_until(clock.live(cx)?));
    poll_fn(|task| {
        clock.live(cx)?;
        let result = future.as_mut().poll(task);
        let at = clock.live(cx)?;
        if result.is_ready() {
            return result;
        }
        if timer.as_mut().poll(task).is_ready() {
            timer.as_mut().reset(after(at, WAKE)?);
            let _ = timer.as_mut().poll(task);
        }
        Poll::Pending
    })
    .await
}
async fn cleanup(cx: Cx, budget: Duration, app: &mut impl Application) -> Result<(), Failure> {
    let mut clock = Clock::new(&cx)?;
    let deadline = Deadline::after(&cx, budget).map_err(|_| Failure::Clock)?;
    let until = deadline.time();
    let mut work = pin!(Owned {
        cx: cx.clone(),
        inner: Box::pin(app.cleanup(&cx, deadline))
    });
    let mut timer = pin!(sleep_until(until));
    poll_fn(|task| {
        if clock.read(&cx)? >= until {
            return Poll::Ready(Err(Failure::CleanupExpired));
        }
        let result = work.as_mut().poll(task);
        if clock.read(&cx)? >= until {
            return Poll::Ready(Err(Failure::CleanupExpired));
        }
        if let Poll::Ready(result) = result {
            return Poll::Ready(result.map_err(|_| Failure::Cleanup));
        }
        let _ = timer.as_mut().poll(task);
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "reconnect/recovery_tests.rs"]
mod recovery_tests;
