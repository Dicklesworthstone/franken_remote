//! A renewable destination identity, never desktop observation/input authority.
//! One owner revalidates; all send-time checks share the same original lifetime.
use crate::{Error, LocalApi, PeerTarget};
use asupersync::{cx::Cx, time::sleep};
use std::{
    fmt,
    future::{Future, poll_fn},
    pin::pin,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
    time::Duration,
};

const REFRESH_INTERVAL_US: u64 = 1_000_000;
struct State {
    target: Arc<PeerTarget>,
    stopped: Option<Error>,
    last_us: u64,
    waiter: Option<Waker>,
}
/// A shared destination check. It cannot select another node, refresh metadata,
/// admit observation, grant input, or keep its owner alive after owner drop.
#[derive(Clone)]
pub struct TargetLease {
    state: Arc<Mutex<State>>,
    api: LocalApi,
    cx: Cx,
}
impl fmt::Debug for TargetLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TargetLease([shared destination lifetime])")
    }
}
impl TargetLease {
    fn live(&self, state: &mut State) -> Result<u64, Error> {
        if let Some(error) = state.stopped {
            return Err(error);
        }
        let result = (|| {
            let now = crate::local::now(&self.cx)?;
            if now < state.last_us {
                return Err(Error::Clock);
            }
            state.last_us = now;
            self.api.check_peer_target(&self.cx, &state.target)?;
            Ok(state.target.expires_us())
        })();
        if let Err(error) = result {
            state.stopped = Some(error);
        }
        result
    }
    /// Check immediately before network admission, not only at discovery/TLS.
    /// Expiry and clock/cancellation failures are sticky, even after refresh.
    pub fn check(&self) -> Result<u64, Error> {
        let (result, wake) = {
            let mut state = self.state.lock().map_err(|_| Error::Revoked)?;
            let result = self.live(&mut state);
            let wake = if result.is_err() {
                state.waiter.take()
            } else {
                None
            };
            (result, wake)
        };
        if let Some(waker) = wake {
            waker.wake();
        }
        result
    }
    pub fn revoke(&self) {
        self.stop(Error::Revoked);
    }
    fn stop(&self, error: Error) {
        let wake = self.state.lock().ok().and_then(|mut state| {
            state.stopped.get_or_insert(error);
            state.waiter.take()
        });
        if let Some(waker) = wake {
            waker.wake();
        }
    }
    fn register(&self, waker: &Waker) -> Result<(), Error> {
        let mut state = self.state.lock().map_err(|_| Error::Revoked)?;
        self.live(&mut state)?;
        if state
            .waiter
            .as_ref()
            .is_none_or(|old| !old.will_wake(waker))
        {
            state.waiter = Some(waker.clone());
        }
        Ok(())
    }
}
/// The sole refresh owner. A connection's application lifetime must be nested
/// inside this owner, with `serve` polled by the existing structured runtime.
/// No listener, socket, native worker, background task or new permission exists.
pub struct TargetOwner {
    lease: TargetLease,
}
impl fmt::Debug for TargetOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TargetOwner([owned destination lifetime])")
    }
}
impl TargetOwner {
    pub fn new(api: LocalApi, cx: Cx, target: PeerTarget) -> Result<Self, Error> {
        api.check_peer_target(&cx, &target)?;
        let last_us = crate::local::now(&cx)?;
        let owner = Self {
            lease: TargetLease {
                state: Arc::new(Mutex::new(State {
                    target: Arc::new(target),
                    stopped: None,
                    last_us,
                    waiter: None,
                })),
                api,
                cx,
            },
        };
        owner.lease.check()?;
        Ok(owner)
    }
    pub fn lease(&self) -> TargetLease {
        self.lease.clone()
    }
    /// Guard creation is synchronous: even an unpolled abandoned refresh stops
    /// this lifetime. The old deadline keeps governing all concurrent sends.
    pub fn refresh(&mut self) -> impl Future<Output = Result<(), Error>> + '_ {
        let guard = Guard {
            lease: self.lease.clone(),
            armed: true,
        };
        async move {
            let mut guard = guard;
            let old = {
                let mut state = self.lease.state.lock().map_err(|_| Error::Revoked)?;
                self.lease.live(&mut state)?;
                state.target.clone()
            };
            let result = self
                .lease
                .api
                .revalidate_peer_target(&self.lease.cx, &old)
                .await;
            let target = match result {
                Ok(target) => target,
                Err(error) => {
                    self.lease.stop(error);
                    return Err(error);
                }
            };
            {
                let mut state = self.lease.state.lock().map_err(|_| Error::Revoked)?;
                self.lease.live(&mut state)?;
                if !Arc::ptr_eq(&old, &state.target) || !old.same_identity(&target) {
                    state.stopped = Some(Error::IdentityChanged);
                    return Err(Error::IdentityChanged);
                }
                state.target = Arc::new(target);
                self.lease.live(&mut state)?;
            }
            guard.armed = false;
            Ok(())
        }
    }
    /// Periodically revalidate the original stable ID. A pending lookup never
    /// holds the gate lock. Explicit stop wakes a stalled lookup without polling
    /// pulses; the single wake slot is replaced rather than accumulated.
    pub fn serve(&mut self) -> impl Future<Output = Result<(), Error>> + '_ {
        let guard = Guard {
            lease: self.lease.clone(),
            armed: true,
        };
        let lease = self.lease.clone();
        async move {
            let _guard = guard;
            let mut service = pin!(async {
                loop {
                    let at = {
                        let mut state = self.lease.state.lock().map_err(|_| Error::Revoked)?;
                        let until = self.lease.live(&mut state)?;
                        state
                            .target
                            .issued_us()
                            .checked_add(REFRESH_INTERVAL_US)
                            .ok_or(Error::Clock)?
                            .min(until)
                    };
                    let timer = self.lease.cx.timer_driver().ok_or(Error::MissingRuntime)?;
                    let start = timer.now();
                    let nanos = at.checked_mul(1000).ok_or(Error::Clock)?;
                    sleep(
                        start,
                        Duration::from_nanos(nanos.saturating_sub(start.as_nanos())),
                    )
                    .await;
                    self.refresh().await?;
                }
            });
            poll_fn(|task| {
                if let Err(error) = lease.register(task.waker()) {
                    return Poll::Ready(Err(error));
                }
                let result = service.as_mut().poll(task);
                if result.is_ready() {
                    lease.revoke();
                }
                result
            })
            .await
        }
    }
}
impl Drop for TargetOwner {
    fn drop(&mut self) {
        self.lease.revoke();
    }
}
struct Guard {
    lease: TargetLease,
    armed: bool,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if self.armed {
            self.lease.revoke();
        }
    }
}
