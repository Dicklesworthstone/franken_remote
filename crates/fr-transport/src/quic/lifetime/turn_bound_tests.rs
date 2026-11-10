//! The drive turn's bound is judged after the turn's own poll. Deterministic:
//! the bound is a flag, the turn counts its polls, nothing reads a clock.
use super::{Error, within_turn_bound};
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    pin::pin,
    task::{Context, Poll, Waker},
};

/// A turn that finishes (with `result`) on poll number `ready_at`.
fn turn(
    polls: &Cell<u32>,
    ready_at: u32,
    result: Result<u32, Error>,
) -> impl Future<Output = Result<u32, Error>> + '_ {
    poll_fn(move |_| {
        polls.set(polls.get() + 1);
        if polls.get() >= ready_at {
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    })
}

/// A bound that is ready once `passed` is set: its deadline has gone by.
fn bound(passed: &Cell<bool>) -> impl Future<Output = ()> + '_ {
    poll_fn(|_| {
        if passed.get() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
}

#[test]
fn a_turn_whose_wait_ended_while_its_thread_was_preempted_past_the_bound_finishes() {
    let (polls, passed) = (Cell::new(0), Cell::new(false));
    let (work, limit) = (pin!(turn(&polls, 2, Ok(7))), pin!(bound(&passed)));
    let mut bounded = pin!(within_turn_bound(work, limit));
    let mut task = Context::from_waker(Waker::noop());
    assert!(bounded.as_mut().poll(&mut task).is_pending());
    // The thread was descheduled past the deadline; the turn's I/O is done.
    passed.set(true);
    assert_eq!(bounded.as_mut().poll(&mut task), Poll::Ready(Ok(7)));
    assert_eq!(polls.get(), 2);
}

#[test]
fn a_turn_still_pending_past_the_bound_expires_after_exactly_one_more_poll() {
    let (polls, passed) = (Cell::new(0), Cell::new(false));
    let (work, limit) = (pin!(turn(&polls, u32::MAX, Ok(7))), pin!(bound(&passed)));
    let mut bounded = pin!(within_turn_bound(work, limit));
    let mut task = Context::from_waker(Waker::noop());
    assert!(bounded.as_mut().poll(&mut task).is_pending());
    assert!(bounded.as_mut().poll(&mut task).is_pending());
    passed.set(true);
    assert_eq!(
        bounded.as_mut().poll(&mut task),
        Poll::Ready(Err(Error::Expired))
    );
    assert_eq!(polls.get(), 3);
}

#[test]
fn a_turn_failure_is_reported_as_itself_even_past_the_bound() {
    let (polls, passed) = (Cell::new(0), Cell::new(true));
    let (work, limit) = (
        pin!(turn(&polls, 1, Err(Error::Native))),
        pin!(bound(&passed)),
    );
    let mut bounded = pin!(within_turn_bound(work, limit));
    let mut task = Context::from_waker(Waker::noop());
    assert_eq!(
        bounded.as_mut().poll(&mut task),
        Poll::Ready(Err(Error::Native))
    );
}
