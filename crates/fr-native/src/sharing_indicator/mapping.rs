//! A bounded prerequisite for capture, not evidence of physical presentation.
use super::{Error, MAP_TIMEOUT, SharingIndicator, Status, StopReason};
use asupersync::{
    cx::Cx,
    time::{TimerDriverHandle, TimerHandle},
    types::Time,
};
use std::{
    future::{Future, poll_fn},
    task::Poll,
};

impl SharingIndicator {
    /// Await the original window's MapNotify/draw submission without blocking
    /// the reactor. Call from the asynchronous native-source factory so the
    /// original Host deadline, admission renewal and OS events remain serviced.
    /// Success is not proof that a human saw the indicator or permission to
    /// capture: keep this owner alive throughout the original source lifetime.
    ///
    /// The deadline starts at CALL time. Missing clocks, cancellation, expiry,
    /// source loss and abandonment (even unpolled) revoke the original source.
    /// The caller retains this owner for explicit, nonblocking native retirement.
    pub fn wait_until_mapped<'a>(
        &'a mut self,
        cx: &'a Cx,
    ) -> impl Future<Output = Result<(), Error>> + Send + 'a {
        let mut waiting = Waiting {
            indicator: self,
            ready: false,
        };
        let clock = cx.timer_driver();
        let initial = clock.as_ref().map(|driver| driver.now().as_nanos());
        async move {
            let start = initial.ok_or_else(|| waiting.fail(StopReason::NativeFailure))?;
            let until = start
                .checked_add(
                    u64::try_from(MAP_TIMEOUT.as_nanos())
                        .map_err(|_| waiting.fail(StopReason::NativeFailure))?,
                )
                .ok_or_else(|| waiting.fail(StopReason::NativeFailure))?;
            let mut pulse = Pulse {
                driver: clock.ok_or_else(|| waiting.fail(StopReason::NativeFailure))?,
                handle: None,
            };
            let mut previous = start;
            poll_fn(|task| {
                cx.checkpoint()
                    .map_err(|_| waiting.fail(StopReason::User))?;
                let tick = pulse.driver.now().as_nanos();
                if tick < previous {
                    return Poll::Ready(Err(waiting.fail(StopReason::NativeFailure)));
                }
                if tick >= until {
                    return Poll::Ready(Err(waiting.fail(StopReason::MappingExpired)));
                }
                previous = tick;
                if !waiting.indicator.control.0.live() {
                    return Poll::Ready(Err(waiting.fail(StopReason::AuthorityEnded)));
                }
                match waiting.indicator.control.status() {
                    Status::Mapped => {
                        waiting.ready = true;
                        return Poll::Ready(Ok(()));
                    }
                    Status::Stopped(reason) => return Poll::Ready(Err(waiting.fail(reason))),
                    Status::Opening => {}
                }
                // Register on the supplied Cx's clock, never an ambient runtime.
                // Cancel/rearm fired handles; only one bounded timer is retained.
                pulse.cancel();
                let wake = Time::from_nanos(tick.saturating_add(10_000_000).min(until));
                pulse.handle = Some(pulse.driver.register(wake, task.waker().clone()));
                if pulse.driver.now() >= wake {
                    task.waker().wake_by_ref();
                }
                Poll::Pending
            })
            .await
        }
    }
}
struct Pulse {
    driver: TimerDriverHandle,
    handle: Option<TimerHandle>,
}
impl Pulse {
    fn cancel(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.driver.cancel(&handle);
        }
    }
}
impl Drop for Pulse {
    fn drop(&mut self) {
        self.cancel();
    }
}

// Created outside the async body: abandoning an unpolled factory is terminal
// too. Never join a possibly blocked native worker from Drop or a reactor poll.
struct Waiting<'a> {
    indicator: &'a mut SharingIndicator,
    ready: bool,
}
impl Waiting<'_> {
    fn fail(&self, reason: StopReason) -> Error {
        self.indicator.control.0.stop(reason);
        match self.indicator.control.status() {
            Status::Stopped(original) => Error::Stopped(original),
            _ => Error::Stopped(reason),
        }
    }
}
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if !self.ready {
            self.indicator.control.0.stop(StopReason::OwnerDropped);
        }
    }
}
#[cfg(test)]
mod tests;
