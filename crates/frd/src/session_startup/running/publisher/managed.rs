//! Own the native input Driver alongside the canonical publisher service.
//! Native calls still run on the existing foreign-call thread; no new runtime,
//! authority, transport, media worker or retry loop is introduced here.
use super::{Error, NativePublisher};
use crate::{
    input_agent::{Driver, Seat, Shutdown, Status},
    input_process::Fence,
    input_quic::grant::Error as GrantError,
    input_watchdog::{Control, StopReason},
    media::ObservationControl,
    session_startup::{HostControlState, PendingHostControl},
};
use fr_core::{
    ids::{InputLeaseId, InputTicketId},
    input_submission::{InputSink, PlatformError},
};
use fr_wire::control::{Request, Target};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll},
};

/// Local approval still requires an explicit decision on the exact current
/// target. Unlike `HostControlState`, approval cannot leak an unpolled Driver.
pub enum ManagedHostControlState<'a> {
    Pending(ManagedPendingControl<'a>),
    Active { request: Request, control: Control },
}

/// The borrowed approval capability never gives the application a native Driver
/// to schedule. A successful approve immediately transfers it into the service.
pub struct ManagedPendingControl<'a> {
    pending: PendingHostControl<'a>,
    native: &'a Mutex<Native>,
}
impl ManagedPendingControl<'_> {
    pub fn request(&self) -> Option<Request> {
        self.pending.request()
    }
    pub fn native_status(&self) -> Option<Status> {
        self.pending.native_status()
    }
    pub fn view_ready(&mut self) -> Result<bool, GrantError> {
        self.pending.view_ready()
    }
    pub fn deny(&mut self) {
        self.pending.deny();
    }
    pub fn stop(&mut self) {
        self.pending.stop();
    }
    /// Same admission, consent, readiness, Seat reservation and initialization
    /// checks as `PendingHostControl::approve`. Neither closure executes under the
    /// driver-slot mutex. No implicit retries of credentials or failed factories.
    pub fn approve<S, F, C>(
        &mut self,
        target: Target,
        fresh: impl FnOnce() -> Option<(InputLeaseId, InputTicketId)>,
        factory: F,
        cleanup: C,
    ) -> Result<(), GrantError>
    where
        S: InputSink + 'static,
        F: FnOnce() -> Result<S, PlatformError> + Send + 'static,
        C: FnMut(&mut S) -> bool + Send + 'static,
    {
        if lock(self.native).started {
            return Err(GrantError::AlreadyGranted);
        }
        let driver = self.pending.approve(target, fresh, factory, cleanup)?;
        // Only this synchronous local callback can install a driver. The outer
        // future polls it only before/after the callback returns, never during it.
        let mut native = lock(self.native);
        native.started = true;
        native.driver = Some(driver);
        Ok(())
    }
    /// `approve` for an out-of-process executor: the lease's `Fence` is
    /// installed on its Control inside this same synchronous callback, before
    /// the outer service can poll any network turn that could queue input, so
    /// every later stop fences the child first. Any refusal signals the fence,
    /// so a factory that has not spawned yet refuses and one that has is fenced.
    pub fn approve_fenced<S, F, C>(
        &mut self,
        target: Target,
        fresh: impl FnOnce() -> Option<(InputLeaseId, InputTicketId)>,
        factory: F,
        cleanup: C,
        fence: &Fence,
    ) -> Result<(), GrantError>
    where
        S: InputSink + 'static,
        F: FnOnce() -> Result<S, PlatformError> + Send + 'static,
        C: FnMut(&mut S) -> bool + Send + 'static,
    {
        if lock(self.native).started {
            fence.signal();
            return Err(GrantError::AlreadyGranted);
        }
        let driver = match self.pending.approve(target, fresh, factory, cleanup) {
            Ok(driver) => driver,
            Err(error) => {
                fence.signal();
                return Err(error);
            }
        };
        let signal = fence.clone();
        driver
            .control()
            .install_fence(Box::new(move || signal.signal()));
        let mut native = lock(self.native);
        native.started = true;
        native.driver = Some(driver);
        Ok(())
    }
}

/// Separate session termination from confirmed native destruction. A session
/// error does not erase successful cleanup; service return never certifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedControlReport {
    pub session: Result<(), Error>,
    /// None means no Driver was obtained by this service, not proof that a
    /// failing native initializer never ran. Some carries the original Driver's
    /// bounded drain result; exit=None means unresolved native work.
    pub input: Option<Shutdown>,
}

#[derive(Default)]
struct Native {
    driver: Option<Driver>,
    report: Option<Shutdown>,
    started: bool,
}
fn lock(native: &Mutex<Native>) -> MutexGuard<'_, Native> {
    // Unwinding a poll still runs Service::drop, which fences before recovering
    // the slot for abandonment. Poisoning never bypasses authority checks.
    native
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
impl Native {
    fn poll(&mut self, task: &mut Context<'_>) {
        if let Some(driver) = &mut self.driver
            && let Poll::Ready(report) = Pin::new(driver).poll(task)
        {
            self.report = Some(report);
            self.driver = None;
        }
    }
}

impl NativePublisher {
    /// Serve control AND its native Driver for the lifetime of this same
    /// publication. The driver gets a poll before and after each network/media
    /// poll, including immediately after approval, and continues its own bounded
    /// drain after session termination. No callback runs during that final drain.
    ///
    /// This constructs the underlying one-use service now. Dropping even an
    /// unpolled future fences the original observation before abandoning work.
    /// The local callback remains bounded/nonblocking and must independently
    /// approve and return the CURRENT qualified target on each call.
    ///
    /// A driver ending first terminates the session with Session(Closed); its
    /// actual reason and cleanup evidence remain in the report. Unresolved
    /// cleanup keeps the original Seat occupied; this wrapper never releases it.
    /// Media reaping and late input receipt collection remain explicit on self.
    pub fn serve_managed_control<'a>(
        &'a mut self,
        seat: Seat,
        mut local: impl FnMut(ManagedHostControlState<'_>) -> Result<Option<Target>, GrantError> + 'a,
        nonce: impl FnMut() -> Result<u128, ()> + 'a,
        ticket: impl FnMut() -> Option<InputTicketId> + 'a,
    ) -> impl Future<Output = ManagedControlReport> + 'a {
        let native = Arc::new(Mutex::new(Native::default()));
        let slot = native.clone();
        let control = self.control();
        let inner = self.serve_accepting_control(
            seat,
            move |state| {
                local(match state {
                    HostControlState::Pending(pending) => {
                        ManagedHostControlState::Pending(ManagedPendingControl {
                            pending,
                            native: &slot,
                        })
                    }
                    HostControlState::Active { request, control } => {
                        ManagedHostControlState::Active { request, control }
                    }
                })
            },
            nonce,
            ticket,
        );
        Service {
            control,
            inner: Some(Box::pin(inner)),
            native,
            session: None,
            done: None,
        }
    }
}

struct Service<F> {
    control: ObservationControl,
    inner: Option<Pin<Box<F>>>,
    native: Arc<Mutex<Native>>,
    session: Option<Result<(), Error>>,
    done: Option<ManagedControlReport>,
}
impl<F> Service<F> {
    fn end(&mut self, result: Result<(), Error>) {
        self.control.revoke();
        self.session = Some(result);
        // Abandoning a still-pending QUIC turn is terminal, and occurs only
        // AFTER revocation. We never cancel a healthy turn for native progress.
        self.inner = None;
    }
    fn native_stopped(&mut self) {
        let stopped = {
            let native = lock(&self.native);
            native.report.is_some()
                || native
                    .driver
                    .as_ref()
                    .is_some_and(|d| d.control().is_stopped())
        };
        if stopped && self.session.is_none() {
            self.end(Err(Error::Session(crate::session_startup::Error::Closed)));
        }
    }
}
impl<F: Future<Output = Result<(), Error>>> Future for Service<F> {
    type Output = ManagedControlReport;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(done) = this.done {
            return Poll::Ready(done);
        }
        lock(&this.native).poll(task);
        this.native_stopped();
        if let Some(inner) = &mut this.inner
            && let Poll::Ready(result) = inner.as_mut().poll(task)
        {
            this.end(result);
        }
        // A callback may have just installed the one driver. Poll it NOW so
        // initialization stalls cannot delay its independent watchdog/drain.
        lock(&this.native).poll(task);
        this.native_stopped();
        let native = lock(&this.native);
        if let Some(session) = this.session
            && native.driver.is_none()
        {
            let report = ManagedControlReport {
                session,
                input: native.report,
            };
            this.done = Some(report);
            return Poll::Ready(report);
        }
        Poll::Pending
    }
}
impl<F> Drop for Service<F> {
    fn drop(&mut self) {
        if self.done.is_none() {
            self.control.revoke();
            let mut native = lock(&self.native);
            if let Some(driver) = &native.driver {
                driver.control().stop(StopReason::Cancelled);
            }
            native.driver = None;
        }
    }
}
