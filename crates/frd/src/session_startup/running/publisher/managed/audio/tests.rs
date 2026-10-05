//! Scheduling/ownership regressions, not native audio or input evidence.
use super::alongside;
use crate::media::shared_publisher::Error;
use std::{
    cell::Cell,
    future::{Future, poll_fn, ready},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

struct Source {
    polls: Rc<Cell<u32>>,
    drops: Rc<Cell<u32>>,
    result: Option<Result<(), Error>>,
}
impl Future for Source {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.polls.set(this.polls.get() + 1);
        match this.result {
            Some(result) => {
                assert_eq!(this.polls.get(), 1, "completed source must not be polled again");
                Poll::Ready(result)
            }
            None => Poll::Pending,
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}
fn source(result: Option<Result<(), Error>>) -> (Source, Rc<Cell<u32>>, Rc<Cell<u32>>) {
    let polls = Rc::new(Cell::new(0));
    let drops = Rc::new(Cell::new(0));
    (
        Source {
            polls: polls.clone(),
            drops: drops.clone(),
            result,
        },
        polls,
        drops,
    )
}

#[test]
fn pending_audio_does_not_delay_session_progress_or_termination() {
    let (audio, audio_polls, drops) = source(None);
    let turns = Cell::new(0);
    let finished = Cell::new(false);
    let session = poll_fn(|_| {
        turns.set(turns.get() + 1);
        if finished.get() {
            Poll::Ready(37)
        } else {
            Poll::Pending
        }
    });
    let mut joined = Box::pin(alongside(session, Some(audio)));
    let mut task = Context::from_waker(Waker::noop());
    for expected in 1..=8 {
        assert!(joined.as_mut().poll(&mut task).is_pending());
        assert_eq!(turns.get(), expected);
        assert_eq!(audio_polls.get(), expected);
    }
    finished.set(true);
    assert_eq!(joined.as_mut().poll(&mut task), Poll::Ready(37));
    // No additional audio work after session termination wins the poll.
    assert_eq!(audio_polls.get(), 8);
    drop(joined);
    assert_eq!(drops.get(), 1);
}

#[test]
fn audio_failure_is_not_a_session_failure_or_a_reason_to_repoll_the_source() {
    for outcome in [Ok(()), Err(Error::Closed)] {
        let (audio, polls, drops) = source(Some(outcome));
        let finished = Cell::new(false);
        let session = poll_fn(|_| {
            if finished.get() {
                Poll::Ready(Err::<(), _>("original session end"))
            } else {
                Poll::Pending
            }
        });
        let mut joined = Box::pin(alongside(session, Some(audio)));
        let mut task = Context::from_waker(Waker::noop());
        for _ in 0..4 {
            assert!(joined.as_mut().poll(&mut task).is_pending());
        }
        assert_eq!(polls.get(), 1);
        assert_eq!(drops.get(), 1);
        finished.set(true);
        assert_eq!(
            joined.as_mut().poll(&mut task),
            Poll::Ready(Err("original session end"))
        );
    }
}

#[test]
fn ready_session_precedes_even_a_ready_audio_source() {
    let (audio, polls, drops) = source(Some(Err(Error::Closed)));
    let mut joined = Box::pin(alongside(ready(11), Some(audio)));
    assert_eq!(
        joined.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(11)
    );
    assert_eq!(polls.get(), 0);
    drop(joined);
    assert_eq!(drops.get(), 1);
}

#[test]
fn dropping_unpolled_service_does_not_start_the_source() {
    let (audio, polls, drops) = source(None);
    drop(alongside(ready(()), Some(audio)));
    assert_eq!(polls.get(), 0);
    assert_eq!(drops.get(), 1);
}

#[test]
fn dropping_pending_service_retires_the_original_source_once() {
    let (audio, polls, drops) = source(None);
    let mut joined = Box::pin(alongside(std::future::pending::<()>(), Some(audio)));
    assert!(joined
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_pending());
    assert_eq!(polls.get(), 1);
    drop(joined);
    assert_eq!(drops.get(), 1);
}

#[test]
fn absence_of_an_audio_profile_adds_no_pending_work() {
    let mut joined = Box::pin(alongside(ready(19), None::<Source>));
    assert_eq!(
        joined.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(19)
    );
}
