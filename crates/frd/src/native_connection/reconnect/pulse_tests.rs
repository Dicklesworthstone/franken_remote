//! Deterministic scheduling faults in the production reconnect supervisor.
//! The pending attempt is a counted future, not a claimed network/native session.
use super::*;
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    time::{TimeSource, TimerDriver, TimerDriverHandle, VirtualClock},
};
use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Wake, Waker},
};

thread_local! {
    // Advance only on the thread manually polling the supervisor, not on
    // background runtime threads. Restore even if the negative control panics.
    static STEP_TIME: Cell<bool> = const { Cell::new(false) };
}
struct Stepping;
impl Stepping {
    fn begin() -> Self {
        STEP_TIME.with(|active| assert!(!active.replace(true)));
        Self
    }
}
impl Drop for Stepping {
    fn drop(&mut self) {
        STEP_TIME.with(|active| active.set(false));
    }
}
#[derive(Debug, Default)]
struct JumpClock(AtomicU64);
impl TimeSource for JumpClock {
    fn now(&self) -> Time {
        let nanos = if STEP_TIME.with(Cell::get) {
            self.0.fetch_add(20_000_000, Ordering::SeqCst)
        } else {
            self.0.load(Ordering::SeqCst)
        };
        Time::from_nanos(nanos)
    }
}
#[derive(Default)]
struct Wakes(AtomicUsize);
impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct State {
    polls: AtomicUsize,
    ready: AtomicBool,
    dropped: AtomicBool,
    fenced_at_drop: AtomicBool,
}
struct Attempt {
    cx: Cx,
    state: Arc<State>,
}
impl Future for Attempt {
    type Output = Result<u32, Failure>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.polls.fetch_add(1, Ordering::SeqCst);
        if self.state.ready.load(Ordering::SeqCst) {
            Poll::Ready(Ok(17))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        self.state
            .fenced_at_drop
            .store(self.cx.is_cancel_requested(), Ordering::SeqCst);
        self.state.dropped.store(true, Ordering::SeqCst);
    }
}
fn runtime(driver: TimerDriverHandle) -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .with_timer_driver(driver)
        .build()
        .unwrap()
}
fn attempt(cx: &Cx, state: &Arc<State>) -> Owned<Attempt> {
    Owned {
        cx: cx.clone(),
        inner: Box::pin(Attempt {
            cx: cx.clone(),
            state: state.clone(),
        }),
    }
}

#[test]
fn repeated_late_pulses_keep_one_attempt_alive_and_cancellable() {
    let source = Arc::new(JumpClock::default());
    let driver = TimerDriverHandle::new(Arc::new(TimerDriver::with_clock(source)));
    let rt = runtime(driver.clone());
    let supervisor = rt.request_cx_with_budget(Budget::INFINITE);
    let child = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut clock = Clock::new(&supervisor).unwrap();
        let state = Arc::new(State::default());
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(wakes.clone());
        let mut task = Context::from_waker(&waker);
        let baseline = driver.pending_count();
        let mut work = Box::pin(guarded(&supervisor, &mut clock, attempt(&child, &state)));
        {
            let _stepping = Stepping::begin();
            for count in 1..=8 {
                // Each clock read advances by more than WAKE. The replacement
                // timer is already due before it can be polled. Discarding its
                // Ready here makes the original implementation panic next turn.
                assert_eq!(work.as_mut().poll(&mut task), Poll::Pending);
                assert_eq!(state.polls.load(Ordering::SeqCst), count);
                assert!(driver.pending_count() <= baseline + 1);
                assert!(!state.dropped.load(Ordering::SeqCst));
            }
        }
        assert!(wakes.0.load(Ordering::SeqCst) >= 8);
        supervisor.cancel_fast(CancelKind::User);
        assert_eq!(
            work.as_mut().poll(&mut task),
            Poll::Ready(Err(Failure::Cancelled))
        );
        drop(work);
        assert_eq!(state.polls.load(Ordering::SeqCst), 8);
        assert!(state.dropped.load(Ordering::SeqCst));
        assert!(state.fenced_at_drop.load(Ordering::SeqCst));
        assert!(child.is_cancel_requested());
        assert_eq!(driver.pending_count(), baseline);
    });
}

#[test]
fn a_ready_attempt_after_late_pulses_completes_once_without_restarting() {
    let driver = TimerDriverHandle::new(Arc::new(TimerDriver::with_clock(Arc::new(
        JumpClock::default(),
    ))));
    let rt = runtime(driver.clone());
    let supervisor = rt.request_cx_with_budget(Budget::INFINITE);
    let child = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut clock = Clock::new(&supervisor).unwrap();
        let state = Arc::new(State::default());
        let mut task = Context::from_waker(Waker::noop());
        let baseline = driver.pending_count();
        let mut work = Box::pin(guarded(&supervisor, &mut clock, attempt(&child, &state)));
        {
            let _stepping = Stepping::begin();
            for _ in 0..4 {
                assert_eq!(work.as_mut().poll(&mut task), Poll::Pending);
            }
            state.ready.store(true, Ordering::SeqCst);
            assert_eq!(work.as_mut().poll(&mut task), Poll::Ready(Ok(17)));
        }
        drop(work);
        assert_eq!(state.polls.load(Ordering::SeqCst), 5);
        assert!(state.fenced_at_drop.load(Ordering::SeqCst));
        assert!(!supervisor.is_cancel_requested());
        assert_eq!(driver.pending_count(), baseline);
    });
}

#[test]
fn an_unexpired_pulse_parks_without_waking_or_accumulating_timers() {
    let source = Arc::new(VirtualClock::new());
    let driver = TimerDriverHandle::with_virtual_clock(source.clone());
    let rt = runtime(driver.clone());
    let supervisor = rt.request_cx_with_budget(Budget::INFINITE);
    let child = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut clock = Clock::new(&supervisor).unwrap();
        let state = Arc::new(State::default());
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(wakes.clone());
        let mut task = Context::from_waker(&waker);
        let baseline = driver.pending_count();
        let mut work = Box::pin(guarded(&supervisor, &mut clock, attempt(&child, &state)));
        assert_eq!(work.as_mut().poll(&mut task), Poll::Pending);
        assert_eq!(work.as_mut().poll(&mut task), Poll::Pending);
        let before = wakes.0.load(Ordering::SeqCst);
        assert_eq!(driver.pending_count(), baseline + 1);
        for _ in 0..16 {
            assert_eq!(work.as_mut().poll(&mut task), Poll::Pending);
        }
        assert_eq!(wakes.0.load(Ordering::SeqCst), before);
        assert_eq!(driver.pending_count(), baseline + 1);
        source.advance_to(Time::from_millis(10));
        driver.process_timers();
        assert_eq!(work.as_mut().poll(&mut task), Poll::Pending);
        assert_eq!(work.as_mut().poll(&mut task), Poll::Pending);
        assert_eq!(driver.pending_count(), baseline + 1);
        drop(work);
        assert!(state.fenced_at_drop.load(Ordering::SeqCst));
        assert_eq!(driver.pending_count(), baseline);
    });
}

#[test]
fn pulse_deadline_overflow_returns_clock_failure_and_fences_the_attempt() {
    let source = Arc::new(VirtualClock::new());
    source.advance_to(Time::from_nanos(u64::MAX - 1));
    let driver = TimerDriverHandle::with_virtual_clock(source);
    let rt = runtime(driver.clone());
    let supervisor = rt.request_cx_with_budget(Budget::INFINITE);
    let child = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut clock = Clock::new(&supervisor).unwrap();
        let state = Arc::new(State::default());
        let baseline = driver.pending_count();
        let mut task = Context::from_waker(Waker::noop());
        let mut work = Box::pin(guarded(&supervisor, &mut clock, attempt(&child, &state)));
        assert_eq!(
            work.as_mut().poll(&mut task),
            Poll::Ready(Err(Failure::Clock))
        );
        drop(work);
        assert_eq!(state.polls.load(Ordering::SeqCst), 1);
        assert!(state.fenced_at_drop.load(Ordering::SeqCst));
        assert_eq!(driver.pending_count(), baseline);
    });
}
