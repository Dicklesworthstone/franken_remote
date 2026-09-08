//! Independently polled input-lease watchdog on the session's Asupersync clock.
//! No native operation runs here. Run this future in the authority region while
//! the input sink runs on its own native thread/process. Dropping even an
//! unpolled watchdog revokes its lease; it never certifies native cleanup.
use asupersync::{
    cx::Cx,
    time::{TimerDriverHandle, TimerHandle},
    types::Time,
};
use fr_core::{input_submission::InputMonitor, time::HostInstant};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    task::{Context, Poll, Waker},
};

/// Maximum interval between cancellation checks. This is not an OS scheduling
/// or native-release latency guarantee. Lease deadlines may wake earlier.
const CHECK_INTERVAL_NS: u64 = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StopReason {
    LocalRevoke = 1,
    AuthorityEnded,
    Cancelled,
    ClockRegression,
    ClockOverflow,
    WatchdogDropped,
    NativeFailure,
    ClientDisconnected,
    ViewInvalidated,
    Suspended,
}
impl StopReason {
    fn from_raw(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::LocalRevoke,
            2 => Self::AuthorityEnded,
            3 => Self::Cancelled,
            4 => Self::ClockRegression,
            5 => Self::ClockOverflow,
            6 => Self::WatchdogDropped,
            7 => Self::NativeFailure,
            8 => Self::ClientDisconnected,
            9 => Self::ViewInvalidated,
            10 => Self::Suspended,
            _ => return None,
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    MissingTimer,
    AlreadyStopped,
}
struct Signal {
    monitor: InputMonitor,
    reason: AtomicU8,
    waker: Mutex<Option<Waker>>,
}
/// Local, revoke-only handle. Clones neither extend deadlines nor grant input.
/// The authority fence happens before waking any task or acquiring a wake lock.
#[derive(Clone)]
pub struct Control(Arc<Signal>);
impl Control {
    pub fn stop(&self, reason: StopReason) {
        self.0.monitor.revoke();
        let _ =
            self.0
                .reason
                .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
        let waker = self
            .0
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
    pub fn is_stopped(&self) -> bool {
        self.0.monitor.is_revoked()
    }
    pub fn reason(&self) -> Option<StopReason> {
        StopReason::from_raw(self.0.reason.load(Ordering::Acquire))
    }
}
impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputControl")
            .field("stopped", &self.is_stopped())
            .field("reason", &self.reason())
            .finish_non_exhaustive()
    }
}

/// Read the SAME timer domain used by `Watchdog`, including on the native thread.
/// Construct the input grant using this clock, not an unrelated Instant origin.
pub fn host_now(cx: &Cx) -> Result<HostInstant, Error> {
    Ok(HostInstant::from_micros(
        cx.timer_driver()
            .ok_or(Error::MissingTimer)?
            .now()
            .as_nanos()
            / 1000,
    ))
}

#[must_use = "poll in the authority region; dropping revokes the input lease"]
pub struct Watchdog {
    cx: Cx,
    driver: TimerDriverHandle,
    timer: Option<(TimerHandle, Time, Waker)>,
    last: Time,
    control: Control,
    done: Option<StopReason>,
}
impl Watchdog {
    /// Takes responsibility synchronously, not on the future's first poll.
    pub fn new(cx: Cx, monitor: InputMonitor) -> Result<Self, Error> {
        let Some(driver) = cx.timer_driver() else {
            monitor.revoke();
            return Err(Error::MissingTimer);
        };
        let last = driver.now();
        if cx.checkpoint().is_err()
            || monitor
                .deadline(HostInstant::from_micros(last.as_nanos() / 1000))
                .is_err()
        {
            monitor.revoke();
            return Err(Error::AlreadyStopped);
        }
        Ok(Self {
            cx,
            driver,
            timer: None,
            last,
            control: Control(Arc::new(Signal {
                monitor,
                reason: AtomicU8::new(0),
                waker: Mutex::new(None),
            })),
            done: None,
        })
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    fn finish(&mut self, reason: StopReason) -> Poll<StopReason> {
        self.control.stop(reason);
        if let Some((timer, _, _)) = self.timer.take() {
            let _ = self.driver.cancel(&timer);
        }
        let reason = self.control.reason().unwrap_or(reason);
        self.done = Some(reason);
        Poll::Ready(reason)
    }
}
impl Future for Watchdog {
    type Output = StopReason;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(reason) = this.done {
            return Poll::Ready(reason);
        }
        // Register before the stop checks so local revoke cannot lose its wake.
        *this
            .control
            .0
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task.waker().clone());
        if this.control.is_stopped() {
            return this.finish(this.control.reason().unwrap_or(StopReason::AuthorityEnded));
        }
        if this.cx.checkpoint().is_err() {
            return this.finish(StopReason::Cancelled);
        }
        let now = this.driver.now();
        if now < this.last {
            return this.finish(StopReason::ClockRegression);
        }
        this.last = now;
        let Ok(deadline) = this
            .control
            .0
            .monitor
            .deadline(HostInstant::from_micros(now.as_nanos() / 1000))
        else {
            return this.finish(StopReason::AuthorityEnded);
        };
        let (Some(deadline), Some(check)) = (
            deadline.as_micros().checked_mul(1000),
            now.as_nanos().checked_add(CHECK_INTERVAL_NS),
        ) else {
            return this.finish(StopReason::ClockOverflow);
        };
        let wake = Time::from_nanos(deadline.min(check));
        // Keep an earlier registration across spurious polls and renewals. A
        // fired handle is inactive: the pinned driver's update() does NOT rearm
        // it. Cancel (whether still active or not), then register afresh.
        let keep = this.timer.as_ref().is_some_and(|(_, at, waker)| {
            now < *at && *at <= wake && waker.will_wake(task.waker())
        });
        if !keep {
            let mut wake = wake;
            if let Some((timer, at, _)) = this.timer.take() {
                let _ = this.driver.cancel(&timer);
                if now < at {
                    wake = wake.min(at);
                }
            }
            this.timer = Some((
                this.driver.register(wake, task.waker().clone()),
                wake,
                task.waker().clone(),
            ));
        }
        if this.control.is_stopped() {
            return this.finish(this.control.reason().unwrap_or(StopReason::AuthorityEnded));
        }
        Poll::Pending
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some((timer, _, _)) = self.timer.take() {
            let _ = self.driver.cancel(&timer);
        }
        if self.done.is_none() {
            self.control.stop(StopReason::WatchdogDropped);
        }
    }
}
