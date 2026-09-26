//! Fixed supervisor-side drain clock. Cancellation does not reset this clock
//! or authorize application work; it only keeps the original cleanup polled.
use asupersync::{
    cx::Cx,
    time::{TimerDriverHandle, TimerHandle},
    types::Time,
};
use std::task::{Context, Waker};

const DRAIN_NS: u64 = 2_000_000_000;

pub(super) struct Budget {
    driver: TimerDriverHandle,
    last: Time,
    until: Time,
    timer: Option<(TimerHandle, Waker)>,
}
impl Budget {
    pub(super) fn new(supervisor: &Cx) -> Result<Self, ()> {
        let driver = supervisor.timer_driver().ok_or(())?;
        let last = driver.now();
        let until = Time::from_nanos(last.as_nanos().checked_add(DRAIN_NS).ok_or(())?);
        Ok(Self {
            driver,
            last,
            until,
            timer: None,
        })
    }
    pub(super) fn expired(&mut self, task: &Context<'_>) -> Result<bool, ()> {
        let now = self.driver.now();
        if now < self.last {
            return Err(());
        }
        self.last = now;
        if now >= self.until {
            return Ok(true);
        }
        if self
            .timer
            .as_ref()
            .is_none_or(|(_, w)| !w.will_wake(task.waker()))
        {
            if let Some((timer, _)) = self.timer.take() {
                let _ = self.driver.cancel(&timer);
            }
            self.timer = Some((
                self.driver.register(self.until, task.waker().clone()),
                task.waker().clone(),
            ));
        }
        Ok(false)
    }
}
impl Drop for Budget {
    fn drop(&mut self) {
        if let Some((timer, _)) = self.timer.take() {
            let _ = self.driver.cancel(&timer);
        }
    }
}
