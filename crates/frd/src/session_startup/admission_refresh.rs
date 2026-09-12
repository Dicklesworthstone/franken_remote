//! Initial negotiation must keep its original UDP owner serviced during `LocalAPI`.
//! This borrows the existing shared admission gate; it creates no observation.
use super::{DENIED, Error, Host, Peer, RETIRED, Role, now};
use asupersync::cx::Cx;
use fr_transport::quic::QuicRecords;
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Clone)]
enum Gate {
    Tailnet(fr_tailnet::Lease),
    #[cfg(test)]
    Fixture {
        alive: Arc<AtomicBool>,
        until: u64,
        control: bool,
    },
}
impl Gate {
    fn check(&self, role: Role) -> Result<u64, Error> {
        match self {
            Self::Tailnet(lease) => if role == Role::RequestControl {
                lease.control()
            } else {
                lease.observe()
            }
            .map_err(Error::Admission),
            #[cfg(test)]
            Self::Fixture {
                alive,
                until,
                control,
            } => {
                if !alive.load(Ordering::Acquire) || (role == Role::RequestControl && !control) {
                    Err(Error::Denied)
                } else {
                    Ok(*until)
                }
            }
        }
    }
    fn revoke(&self) {
        match self {
            Self::Tailnet(lease) => lease.revoke(),
            #[cfg(test)]
            Self::Fixture { alive, .. } => {
                alive.store(false, Ordering::Release);
            }
        }
    }
}

/// Copy only handles before exclusively borrowing the refresh owner. Permission
/// remains in its one shared gate, not in a new `SessionAuthority` or proof copy.
pub(super) struct Bound {
    gate: Gate,
    decision: Arc<AtomicU8>,
    cx: Cx,
    role: Role,
    until: u64,
    peer_until: u64,
    last: AtomicU64,
    refreshed: AtomicBool,
    failure: Mutex<Option<Error>>,
}
impl Bound {
    pub(super) fn new(host: &Host, peer_until: u64) -> Result<Self, Error> {
        let gate = match host.peer.as_ref().ok_or(Error::Closed)? {
            Peer::Tailnet(owner) => Gate::Tailnet(owner.lease()),
            #[cfg(test)]
            Peer::Fixture {
                alive,
                until,
                control,
            } => Gate::Fixture {
                alive: alive.clone(),
                until: *until,
                control: *control,
            },
        };
        let bound = Self {
            gate,
            decision: host.approval.clone(),
            cx: host.cx.clone(),
            role: host.role,
            until: host
                .observation_until
                .map_or(host.until, |d| d.min(host.until)),
            peer_until,
            last: AtomicU64::new(now(&host.cx)?),
            refreshed: AtomicBool::new(false),
            failure: Mutex::new(None),
        };
        bound.check()?;
        Ok(bound)
    }
    fn fail(&self, error: Error) -> Error {
        let error = self
            .failure
            .lock()
            .map_or(Error::Closed, |mut first| *first.get_or_insert(error));
        self.decision.store(RETIRED, Ordering::Release);
        self.gate.revoke();
        error
    }
    fn check(&self) -> Result<u64, Error> {
        // A transport authorization callback can discover expiry first. Keep
        // that cause instead of reporting its resulting retirement as denial.
        let failure = self.failure.lock().map_or(Some(Error::Closed), |f| *f);
        if let Some(error) = failure {
            return Err(error);
        }
        self.check_inner().map_err(|error| self.fail(error))
    }
    fn check_inner(&self) -> Result<u64, Error> {
        if matches!(self.decision.load(Ordering::Acquire), DENIED | RETIRED) {
            return Err(Error::Denied);
        }
        let current = now(&self.cx)?;
        if current < self.last.fetch_max(current, Ordering::AcqRel) {
            return Err(Error::Clock);
        }
        if current >= self.until
            || (!self.refreshed.load(Ordering::Acquire) && current >= self.peer_until)
            || current >= self.gate.check(self.role)?
        {
            return Err(Error::Expired);
        }
        Ok(current)
    }
    /// Construct the fence before returning the future. Errors, unwind and even
    /// unpolled abandonment retire consent and admission before dropping I/O.
    pub(super) fn run<'a>(
        self,
        transport: &'a mut QuicRecords,
        refresh: impl Future<Output = Result<(), Error>> + 'a,
        wait: Duration,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        Operation {
            gate: self.gate.clone(),
            decision: self.decision.clone(),
            complete: false,
            inner: Box::pin(async move { self.pump(transport, refresh, wait).await }),
        }
    }
    async fn pump(
        &self,
        transport: &mut QuicRecords,
        refresh: impl Future<Output = Result<(), Error>>,
        wait: Duration,
    ) -> Result<(), Error> {
        let mut refresh = pin!(refresh);
        loop {
            let current = self.check()?;
            let remaining = self.until.min(self.peer_until) - current;
            // A zero-time caller still cooperatively waits during lookup; the
            // original proof and startup budgets, not poll cadence, bound it.
            let wait = wait
                .max(Duration::from_millis(1))
                .min(Duration::from_micros(remaining));
            {
                let mut io = pin!(transport.drive(&self.cx, wait, || self.check().is_ok()));
                poll_fn(|task| {
                    self.check()?;
                    if !self.refreshed.load(Ordering::Acquire)
                        && let Poll::Ready(result) = refresh.as_mut().poll(task)
                    {
                        result.map_err(|error| self.fail(error))?;
                        // A successful lookup may have advanced the shared gate,
                        // but cannot validate a response after the OLD expiry.
                        self.check()?;
                        self.refreshed.store(true, Ordering::Release);
                    }
                    self.check()?;
                    let result = io.as_mut().poll(task);
                    self.check()?;
                    result.map_err(|error| self.fail(Error::Transport(error)))
                })
                .await?;
            }
            if self.refreshed.load(Ordering::Acquire) {
                return Ok(());
            }
            asupersync::runtime::yield_now().await;
        }
    }
}
struct Operation<F> {
    gate: Gate,
    decision: Arc<AtomicU8>,
    complete: bool,
    inner: Pin<Box<F>>,
}
impl<F> Operation<F> {
    fn stop(&self) {
        self.decision.store(RETIRED, Ordering::Release);
        self.gate.revoke();
    }
}
impl<F: Future<Output = Result<(), Error>>> Future for Operation<F> {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let outcome = this.inner.as_mut().poll(task);
        if let Poll::Ready(ref result) = outcome {
            this.complete = result.is_ok();
            if !this.complete {
                this.stop();
            }
        }
        outcome
    }
}
impl<F> Drop for Operation<F> {
    fn drop(&mut self) {
        if !self.complete {
            self.stop();
        }
    }
}
