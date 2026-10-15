//! Poll-order and ownership tests of the production join, not transport or
//! codec simulations. The existing outer serving guard owns abandonment.
use super::*;
use asupersync::{runtime::RuntimeBuilder, types::Budget};
use std::{
    cell::Cell,
    rc::Rc,
    task::{Context, Waker},
};

fn control() -> (asupersync::runtime::Runtime, StreamingViewerControl) {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let control =
        StreamingViewerControl::observing(runtime.request_cx_with_budget(Budget::INFINITE));
    (runtime, control)
}

struct Native {
    control: StreamingViewerControl,
    ready: Rc<Cell<bool>>,
    polls: Rc<Cell<u32>>,
    dropped: Rc<Cell<Option<bool>>>,
    result: Result<u64, media::Error>,
}
impl Future for Native {
    type Output = Result<u64, media::Error>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.set(self.polls.get() + 1);
        if self.ready.get() {
            Poll::Ready(self.result)
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        self.dropped.set(Some(self.control.is_stopped()));
    }
}
struct Turn {
    control: StreamingViewerControl,
    ready: Rc<Cell<bool>>,
    completed: Rc<Cell<bool>>,
    dropped: Rc<Cell<Option<bool>>>,
    requires_fence: bool,
    result: Result<(), Error>,
}
impl Future for Turn {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if self.requires_fence {
            assert!(
                self.control.is_stopped(),
                "native failure must fence before network polling"
            );
        }
        assert!(!self.completed.get(), "completed network turn polled again");
        if self.ready.get() {
            self.completed.set(true);
            Poll::Ready(self.result)
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Turn {
    fn drop(&mut self) {
        self.dropped.set(Some(self.control.is_stopped()));
    }
}
fn native(
    control: &StreamingViewerControl,
    ready: bool,
    result: Result<u64, media::Error>,
) -> Native {
    Native {
        control: control.clone(),
        ready: Rc::new(Cell::new(ready)),
        polls: Rc::new(Cell::new(0)),
        dropped: Rc::new(Cell::new(None)),
        result,
    }
}
fn turn(control: &StreamingViewerControl, ready: bool, requires_fence: bool) -> Turn {
    Turn {
        control: control.clone(),
        ready: Rc::new(Cell::new(ready)),
        completed: Rc::new(Cell::new(false)),
        dropped: Rc::new(Cell::new(None)),
        requires_fence,
        result: Ok(()),
    }
}
fn poll<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn native_success_waits_for_original_network_turn_without_repolling_completion() {
    let (_runtime, control) = control();
    let mut native = Box::pin(native(&control, true, Ok(7)));
    let polls = native.polls.clone();
    let network = turn(&control, false, false);
    let ready = network.ready.clone();
    let complete = network.completed.clone();
    let mut joined = Box::pin(drive(native.as_mut(), network, &control));
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    assert_eq!(polls.get(), 1);
    assert!(!complete.get());
    assert!(!control.is_stopped());
    ready.set(true);
    assert_eq!(poll(joined.as_mut()), Poll::Ready(Ok(Some(7))));
    assert!(complete.get());
    assert!(!control.is_stopped());
}

#[test]
fn native_failure_fences_then_drains_the_same_network_future_and_keeps_its_cause() {
    let (_runtime, control) = control();
    let mut native = Box::pin(native(&control, true, Err(media::Error::InvalidFrame)));
    let polls = native.polls.clone();
    let mut network = turn(&control, false, true);
    network.result = Err(Error::Closed); // Cancellation observed by the original drive.
    let ready = network.ready.clone();
    let complete = network.completed.clone();
    let dropped = network.dropped.clone();
    let mut joined = Box::pin(drive(native.as_mut(), network, &control));
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    assert!(control.is_stopped());
    assert_eq!(
        dropped.get(),
        None,
        "in-flight drive abandoned instead of drained"
    );
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    assert_eq!(polls.get(), 1);
    ready.set(true);
    assert_eq!(
        poll(joined.as_mut()),
        Poll::Ready(Err(Error::Media(media::Error::InvalidFrame)))
    );
    assert!(complete.get());
    drop(joined);
    assert_eq!(dropped.get(), Some(true));
}

#[test]
fn network_failure_fences_before_the_pending_native_owner_is_dropped() {
    let (_runtime, control) = control();
    let mut native = Box::pin(native(&control, false, Ok(7)));
    let dropped = native.dropped.clone();
    let mut network = turn(&control, true, false);
    network.result = Err(Error::Application);
    let mut joined = Box::pin(drive(native.as_mut(), network, &control));
    assert_eq!(poll(joined.as_mut()), Poll::Ready(Err(Error::Application)));
    assert!(control.is_stopped());
    assert_eq!(dropped.get(), None);
    drop(joined);
    drop(native);
    assert_eq!(dropped.get(), Some(true));
}

#[test]
fn pending_native_work_survives_successive_turns_without_a_second_owner() {
    let (_runtime, control) = control();
    let mut native = Box::pin(native(&control, false, Ok(19)));
    for _ in 0..3 {
        let network = turn(&control, true, false);
        let complete = network.completed.clone();
        let mut joined = Box::pin(drive(native.as_mut(), network, &control));
        assert_eq!(poll(joined.as_mut()), Poll::Ready(Ok(None)));
        assert!(complete.get());
        assert!(!control.is_stopped());
    }
    assert_eq!(native.polls.get(), 3);
    assert_eq!(native.dropped.get(), None);
    native.ready.set(true);
    let mut joined = Box::pin(drive(
        native.as_mut(),
        turn(&control, true, false),
        &control,
    ));
    assert_eq!(poll(joined.as_mut()), Poll::Ready(Ok(Some(19))));
    assert!(!control.is_stopped());
}

#[test]
fn serving_guard_fences_unpolled_or_pending_join_before_abandonment() {
    for polled in [false, true] {
        let (_runtime, control) = control();
        let mut native = Box::pin(native(&control, false, Ok(7)));
        let native_dropped = native.dropped.clone();
        let network = turn(&control, false, false);
        let network_dropped = network.dropped.clone();
        let mut guarded = Box::pin(super::super::Guarded {
            fence: control.clone(),
            inner: Box::pin(drive(native.as_mut(), network, &control)),
        });
        if polled {
            assert_eq!(poll(guarded.as_mut()), Poll::Pending);
        }
        assert!(!control.is_stopped());
        drop(guarded);
        assert_eq!(network_dropped.get(), Some(true));
        drop(native);
        assert_eq!(native_dropped.get(), Some(true));
    }
}
