//! First-source creation AFTER the original observer's actual admission/consent.
use super::{ConsentError, Error, LocalAction, Renewal, SessionAgent, Wake};
use crate::{
    media::{ObservationControl, shared_publisher::Publisher},
    session_agent::source::prepare::{Prepared, Setup},
    session_startup::{
        Approval, Host, HostSession,
        shared_viewers::{Admission, Entropy, Hub, Policy, Ticket},
    },
};
use asupersync::{
    cx::Cx,
    types::{CancelKind, Time},
};
use fr_media::worker::Configuration;
use fr_transport::quic::Disposition;
use fr_wire::{
    display::{Catalog, Select},
    negotiation::Role,
};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

/// Owns the original selected capture child and its first shared-viewer hub.
/// Keep this on the OS source task, independently of the first viewer's task.
/// Opening succeeds before decoder completion; it does NOT establish visibility.
/// Retain the launch's Retirement separately to confirm cleanup after abandonment.
pub struct NativeDesktop {
    hub: Hub,
    prepared: Prepared,
}
impl std::fmt::Debug for NativeDesktop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeDesktop([original source and viewer hub])")
    }
}
impl NativeDesktop {
    pub fn admissions(&self) -> Admission {
        self.hub.admissions()
    }
    pub fn first(&self) -> Ticket {
        self.hub.initial()
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.prepared.worker_id()
    }
    /// Borrow the same owners for `SessionAgent::serve_shared_desktop` and reaping.
    pub fn parts(&mut self) -> (&mut Publisher, &mut Hub) {
        (&mut self.prepared.publisher, &mut self.hub)
    }
    pub fn close(&mut self) {
        self.hub.close();
        self.prepared.publisher.close();
    }
}
impl Drop for NativeDesktop {
    fn drop(&mut self) {
        self.close();
    }
}

impl SessionAgent {
    /// Negotiate a first admitted Host, wait for actual local approval, THEN
    /// construct independent source authority and prepare the original native
    /// child. Display choice, attachments and decoder startup use the same Host
    /// and its original absolute deadline; no warm capture spans approval waits.
    ///
    /// factory is one bounded/nonblocking LOCAL operation. It must return an
    /// independently authorized source, protected package launch and bounded pool,
    /// never derive them from peer paths or the peer's observation authority.
    /// Retain `Launch::retain_cleanup`'s `Retirement` outside this future. select is
    /// likewise local. No OS permission is inferred: local must feed real platform
    /// events/probes and retain responsibility for any held-input cleanup.
    ///
    /// Native preparation and original session renewal run concurrently. Native
    /// success drains the pending network turn instead of abandoning it. Missing
    /// permission, expiry, local Stop, failure, panic and unpolled abandonment
    /// fence owned authority BEFORE dropping pending network/native work.
    ///
    /// Drive this on the independent OS-source owner; do not return its result
    /// from a native connection-scoped callback whose exit cancels the first peer.
    #[allow(clippy::too_many_arguments)]
    pub fn open_native_shared_desktop<'a, F, S, N, L>(
        &'a mut self,
        mut first: Host,
        factory: F,
        select: S,
        policy: Policy,
        entropy: Entropy,
        notify: N,
        mut local: L,
    ) -> Result<
        impl Future<Output = Result<NativeDesktop, Error>> + Send + use<'a, F, S, N, L>,
        Error,
    >
    where
        F: FnOnce() -> Result<Setup, ()> + Send + 'a,
        S: FnOnce(&Catalog) -> Result<(Select, Configuration), ()> + Send + 'a,
        N: FnMut(Approval, Role) -> Result<(), ()> + Send + 'a,
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
    {
        let mut fence = Fence(Some(first.cancellation_context()));
        policy.validate().map_err(Error::Viewers)?;
        let (peer, binding, until) = first.restrict_shared_observer().map_err(Error::Startup)?;
        let os = self.permissions().os_session_id();
        if binding.os_session.as_raw() != u128::from(os) {
            return Err(Error::Consent(ConsentError::SessionChanged));
        }
        Renewal::permission(self, os).map_err(Error::Consent)?;
        peer.timer_driver().ok_or(Error::Clock)?;
        let source = Arc::new(Mutex::new(None));
        let owned = source.clone();
        let deadline = Startup {
            peer: peer.clone(),
            os,
            until,
        };
        let inner = Box::pin(async move {
            let mut session = local_stage(self, &deadline, &mut local, None, &entropy, async {
                first
                    .open(Duration::from_millis(5), notify)
                    .await
                    .map_err(Error::Startup)
            })
            .await?;
            session.check().map_err(Error::Startup)?;
            let setup = factory().map_err(|()| Error::SourceSetup)?;
            let mut prepared = prepare_during_session(
                self,
                &mut session,
                &deadline,
                &entropy,
                &owned,
                setup,
                select,
                &mut local,
            )
            .await?;
            deadline.check()?;
            let registration = self
                .register_original_source(&prepared.publisher)
                .map_err(Error::Consent)?;
            let random = entropy.clone();
            let opening = session.start_shared_display_capped(
                &mut prepared.publisher,
                &prepared.initial,
                policy.join_timeout,
                until,
                policy.send,
                move || random(),
            );
            let shared = local_stage(
                self,
                &deadline,
                &mut local,
                Some(&registration),
                &entropy,
                async { opening.await.map_err(Error::Publication) },
            )
            .await?;
            let hub = Hub::new(shared, policy, entropy).map_err(Error::Viewers)?;
            Ok(NativeDesktop { hub, prepared })
        });
        fence.0 = None;
        Ok(Opening {
            peer,
            source,
            inner: Some(inner),
            finished: false,
        })
    }
}

struct Startup {
    peer: Cx,
    os: u32,
    until: u64,
}
impl Startup {
    fn check(&self) -> Result<u64, Error> {
        self.peer
            .checkpoint()
            .map_err(|_| Error::Startup(crate::session_startup::Error::Cancelled))?;
        let now = crate::media::host_now(&self.peer)
            .map_err(|_| Error::Clock)?
            .as_micros();
        if now >= self.until {
            return Err(Error::Startup(crate::session_startup::Error::Expired));
        }
        Ok(now)
    }
}
async fn local_stage<T, L>(
    agent: &mut SessionAgent,
    startup: &Startup,
    local: &mut L,
    registration: Option<&Arc<Renewal>>,
    entropy: &Entropy,
    work: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error>
where
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()>,
{
    let mut work = pin!(work);
    let mut previous = startup.check()?;
    let mut timer = Wake {
        driver: startup.peer.timer_driver().ok_or(Error::Clock)?,
        handle: None,
    };
    poll_fn(|task| {
        let now = startup.check()?;
        if now < previous {
            return Poll::Ready(Err(Error::Clock));
        }
        previous = now;
        Renewal::permission(agent, startup.os).map_err(Error::Consent)?;
        if local(agent, task).map_err(|()| Error::LocalEvent)? == LocalAction::Stop {
            return Poll::Ready(Err(Error::Closed));
        }
        Renewal::permission(agent, startup.os).map_err(Error::Consent)?;
        if let Some(registration) = registration {
            registration
                .service(agent, &mut || entropy())
                .map_err(Error::Consent)?;
        }
        startup.check()?;
        let result = work.as_mut().poll(task);
        // Preserve an actual refusal instead of masking it with its own cleanup.
        if let Poll::Ready(Err(error)) = result {
            return Poll::Ready(Err(error));
        }
        let now = startup.check()?;
        Renewal::permission(agent, startup.os).map_err(Error::Consent)?;
        if let Some(registration) = registration {
            registration.recheck(agent).map_err(Error::Consent)?;
        }
        if result.is_pending() {
            timer.arm(
                Time::from_nanos(
                    now.saturating_add(10_000)
                        .min(startup.until)
                        .saturating_mul(1000),
                ),
                task,
            );
        }
        result
    })
    .await
}
// Drain a healthy in-flight network turn while continuing actual local consent
// checks after native SUCCESS. Never drop/restart that turn just to regain access
// to the agent: a stalled admission refresh must not suppress local revoke.
#[allow(clippy::too_many_arguments)]
async fn prepare_during_session<S, L>(
    agent: &mut SessionAgent,
    session: &mut HostSession,
    startup: &Startup,
    entropy: &Entropy,
    owned: &Source,
    setup: Setup,
    select: S,
    local: &mut L,
) -> Result<Prepared, Error>
where
    S: FnOnce(&Catalog) -> Result<(Select, Configuration), ()> + Send,
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send,
{
    let completed = AtomicBool::new(false);
    let mut network = Box::pin(async {
        while !completed.load(Ordering::Acquire) {
            session
                .drive(
                    Duration::from_millis(5),
                    || entropy(),
                    |_, _| Ok(Disposition::Blocked),
                )
                .await
                .map_err(Error::Startup)?;
        }
        Ok(())
    });
    let prepared = {
        let control = setup.control.clone();
        // A refusal cannot revoke a source registered to a different agent.
        let work = agent
            .prepare_native_shared_source(setup, select, &mut *local)
            .map_err(Error::Preparation)?;
        *owned.lock().map_err(|_| Error::Closed)? = Some(control);
        let mut work = pin!(work);
        let mut previous = startup.check()?;
        poll_fn(|task| {
            let now = startup.check()?;
            if now < previous {
                return Poll::Ready(Err(Error::Clock));
            }
            previous = now;
            match work.as_mut().poll(task) {
                Poll::Ready(result) => {
                    completed.store(true, Ordering::Release);
                    Poll::Ready(result.map_err(Error::Preparation))
                }
                Poll::Pending => {
                    startup.check()?;
                    match network.as_mut().poll(task) {
                        Poll::Ready(result) => {
                            Poll::Ready(Err(result.err().unwrap_or(Error::Closed)))
                        }
                        Poll::Pending => Poll::Pending,
                    }
                }
            }
        })
        .await?
    }; // Release the preparation's agent/local borrow, not the network future.
    let registration = agent
        .register_original_source(&prepared.publisher)
        .map_err(Error::Consent)?;
    local_stage(agent, startup, local, Some(&registration), entropy, network).await?;
    Ok(prepared)
}

struct Fence(Option<Cx>);
impl Drop for Fence {
    fn drop(&mut self) {
        if let Some(peer) = &self.0 {
            peer.cancel_fast(CancelKind::User);
        }
    }
}

type Source = Arc<Mutex<Option<ObservationControl>>>;
type Work<'a> = Pin<Box<dyn Future<Output = Result<NativeDesktop, Error>> + Send + 'a>>;
struct Opening<'a> {
    peer: Cx,
    source: Source,
    inner: Option<Work<'a>>,
    finished: bool,
}
impl Opening<'_> {
    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.peer.cancel_fast(CancelKind::User);
            if let Some(source) = self
                .source
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                source.revoke();
            }
            drop(self.inner.take());
        }
    }
}
impl Future for Opening<'_> {
    type Output = Result<NativeDesktop, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let mut guard = Turn {
            opening: self.get_mut(),
            completed: false,
        };
        if guard.opening.finished {
            guard.completed = true;
            return Poll::Ready(Err(Error::Closed));
        }
        let result = guard
            .opening
            .inner
            .as_mut()
            .ok_or(Error::Closed)?
            .as_mut()
            .poll(task);
        if matches!(result, Poll::Ready(Ok(_))) {
            guard.opening.finished = true;
            drop(guard.opening.inner.take());
        } else if result.is_ready() {
            guard.opening.finish();
        }
        guard.completed = true;
        result
    }
}
struct Turn<'r, 'a> {
    opening: &'r mut Opening<'a>,
    completed: bool,
}
impl Drop for Turn<'_, '_> {
    fn drop(&mut self) {
        if !self.completed {
            self.opening.finish();
        }
    }
}
impl Drop for Opening<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}
