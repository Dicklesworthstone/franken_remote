//! Actual IPC guard and readiness/wakeup machinery with controlled authority
//! and virtual time. The inner futures are explicit stand-ins for pipe I/O.
use super::*;
use asupersync::{
    runtime::RuntimeBuilder,
    time::{TimerDriverHandle, VirtualClock},
};
use std::{
    cell::{Cell, RefCell},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Wake, Waker},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardError {
    Worker(Error),
    Revoked,
}
impl From<Error> for GuardError {
    fn from(error: Error) -> Self {
        Self::Worker(error)
    }
}
fn runtime(clock: &Arc<VirtualClock>) -> (Runtime, TimerDriverHandle) {
    let driver = TimerDriverHandle::with_virtual_clock(Arc::clone(clock));
    let runtime = RuntimeBuilder::current_thread()
        .with_timer_driver(driver.clone())
        .build()
        .unwrap();
    (runtime, driver)
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

#[test]
fn refreshed_live_cap_allows_completion_past_the_old_cap_without_idle_io_polls() {
    let clock = Arc::new(VirtualClock::new());
    let (runtime, _) = runtime(&clock);
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(2)).unwrap();
        let authority_until = Cell::new(Time::from_millis(600));
        let ready = Cell::new(false);
        let polls = Cell::new(0);
        let native_waker = RefCell::new(None);
        let mut work = pin!(bounded(
            &cx,
            deadline,
            || Ok::<_, GuardError>(authority_until.get()),
            poll_fn(|task| {
                polls.set(polls.get() + 1);
                *native_waker.borrow_mut() = Some(task.waker().clone());
                if ready.get() {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
        ));
        poll_fn(|task| {
            assert!(work.as_mut().poll(task).is_pending());
            clock.advance_to(Time::from_millis(500));
            authority_until.set(Time::from_millis(1500));
            assert!(work.as_mut().poll(task).is_pending());
            clock.advance_to(Time::from_millis(700));
            assert!(work.as_mut().poll(task).is_pending());
            assert_eq!(polls.get(), 1, "supervision must not busy-poll idle IPC");
            ready.set(true);
            native_waker.borrow().as_ref().unwrap().wake_by_ref();
            assert_eq!(work.as_mut().poll(task), Poll::Ready(Ok(())));
            assert_eq!(polls.get(), 2);
            Poll::Ready(())
        })
        .await;
    });
}

#[test]
fn refreshing_authority_cannot_move_the_original_work_ceiling() {
    let clock = Arc::new(VirtualClock::new());
    let (runtime, _) = runtime(&clock);
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(2)).unwrap();
        let authority_until = Cell::new(Time::from_millis(1000));
        let polls = Cell::new(0);
        let mut work = pin!(bounded(
            &cx,
            deadline,
            || Ok::<_, GuardError>(authority_until.get()),
            poll_fn(|_| {
                polls.set(polls.get() + 1);
                Poll::<Result<(), Error>>::Pending
            })
        ));
        poll_fn(|task| {
            assert!(work.as_mut().poll(task).is_pending());
            clock.advance_to(Time::from_millis(900));
            authority_until.set(Time::from_millis(5000));
            assert!(work.as_mut().poll(task).is_pending());
            clock.advance_to(Time::from_millis(2000));
            assert_eq!(
                work.as_mut().poll(task),
                Poll::Ready(Err(GuardError::Worker(Error::Deadline)))
            );
            assert_eq!(polls.get(), 1);
            Poll::Ready(())
        })
        .await;
    });
}

#[test]
fn the_actual_authority_expiry_wakes_before_the_ten_millisecond_watchdog() {
    for shorten_after_registration in [false, true] {
        let clock = Arc::new(VirtualClock::new());
        let (runtime, driver) = runtime(&clock);
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            let deadline = Deadline::after(&cx, Duration::from_secs(2)).unwrap();
            let authority_until = Cell::new(Time::from_millis(if shorten_after_registration {
                1000
            } else {
                3
            }));
            let polls = Cell::new(0);
            let wakes = Arc::new(Wakes::default());
            let waker = Waker::from(Arc::clone(&wakes));
            let mut task = Context::from_waker(&waker);
            let mut work = pin!(bounded(
                &cx,
                deadline,
                || Ok::<_, GuardError>(authority_until.get()),
                poll_fn(|_| {
                    polls.set(polls.get() + 1);
                    Poll::<Result<(), Error>>::Pending
                })
            ));
            // First poll rearms the initially elapsed pulse; second registers it.
            assert!(work.as_mut().poll(&mut task).is_pending());
            assert!(work.as_mut().poll(&mut task).is_pending());
            if shorten_after_registration {
                authority_until.set(Time::from_millis(3));
                assert!(work.as_mut().poll(&mut task).is_pending());
            }
            let before = wakes.0.load(Ordering::SeqCst);
            clock.advance_to(Time::from_millis(2));
            let _ = driver.process_timers();
            assert_eq!(wakes.0.load(Ordering::SeqCst), before);
            clock.advance_to(Time::from_millis(3));
            let _ = driver.process_timers();
            assert!(wakes.0.load(Ordering::SeqCst) > before);
            assert_eq!(
                work.as_mut().poll(&mut task),
                Poll::Ready(Err(GuardError::Worker(Error::Deadline)))
            );
            assert_eq!(polls.get(), 1);
        });
    }
}

#[test]
fn typed_revocation_is_preserved_before_any_native_io() {
    let clock = Arc::new(VirtualClock::new());
    let (runtime, _) = runtime(&clock);
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(2)).unwrap();
        let polls = Cell::new(0);
        let result = bounded(
            &cx,
            deadline,
            || Err(GuardError::Revoked),
            poll_fn(|_| {
                polls.set(polls.get() + 1);
                Poll::Ready(Ok::<(), Error>(()))
            }),
        )
        .await;
        assert_eq!(result, Err(GuardError::Revoked));
        assert_eq!(polls.get(), 0);
    });
}

#[test]
fn revocation_during_pending_or_ready_native_poll_is_terminal() {
    for complete in [false, true] {
        let clock = Arc::new(VirtualClock::new());
        let (runtime, _) = runtime(&clock);
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            let deadline = Deadline::after(&cx, Duration::from_secs(2)).unwrap();
            let admitted = Cell::new(true);
            let result = bounded(
                &cx,
                deadline,
                || {
                    if admitted.get() {
                        Ok(deadline.time())
                    } else {
                        Err(GuardError::Revoked)
                    }
                },
                poll_fn(|_| {
                    admitted.set(false);
                    if complete {
                        Poll::Ready(Ok::<(), Error>(()))
                    } else {
                        Poll::Pending
                    }
                }),
            )
            .await;
            assert_eq!(result, Err(GuardError::Revoked));
        });
    }
}

#[test]
fn time_spent_in_the_authority_callback_is_checked_before_native_io() {
    let clock = Arc::new(VirtualClock::new());
    let (runtime, _) = runtime(&clock);
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(2)).unwrap();
        let polls = Cell::new(0);
        let result = bounded(
            &cx,
            deadline,
            || {
                clock.advance_to(Time::from_millis(3));
                Ok::<_, GuardError>(Time::from_millis(3))
            },
            poll_fn(|_| {
                polls.set(polls.get() + 1);
                Poll::Ready(Ok::<(), Error>(()))
            }),
        )
        .await;
        assert_eq!(result, Err(GuardError::Worker(Error::Deadline)));
        assert_eq!(polls.get(), 0);
    });
}

#[test]
fn a_ready_native_result_cannot_cross_the_original_work_deadline() {
    let clock = Arc::new(VirtualClock::new());
    let (runtime, _) = runtime(&clock);
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(2)).unwrap();
        let result = bounded(
            &cx,
            deadline,
            || Ok::<_, GuardError>(Time::from_millis(5000)),
            poll_fn(|_| {
                clock.advance_to(deadline.time());
                Poll::Ready(Ok::<(), Error>(()))
            }),
        )
        .await;
        assert_eq!(result, Err(GuardError::Worker(Error::Deadline)));
    });
}
