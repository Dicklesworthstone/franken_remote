//! Deterministic poll/ownership tests of the production recovery join. The
//! deadline and futures here are explicit schedules, not QUIC or codec claims.
use super::super::decoder_startup;
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
fn expired() -> Error {
    Error::Startup(decoder_startup::Error::Expired)
}
fn permission(live: &Cell<bool>, control: &StreamingViewerControl) -> Result<(), Error> {
    if control.is_stopped() {
        Err(Error::Closed)
    } else if live.get() {
        Ok(())
    } else {
        Err(expired())
    }
}
fn poll<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
}

struct Native {
    control: StreamingViewerControl,
    ready: Rc<Cell<bool>>,
    polls: Rc<Cell<u32>>,
    dropped: Rc<Cell<Option<bool>>>,
    result: Result<u64, decoder_startup::Error>,
}
impl Future for Native {
    type Output = Result<u64, decoder_startup::Error>;
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
fn native(control: &StreamingViewerControl, ready: bool) -> Native {
    Native {
        control: control.clone(),
        ready: Rc::new(Cell::new(ready)),
        polls: Rc::new(Cell::new(0)),
        dropped: Rc::new(Cell::new(None)),
        result: Ok(17),
    }
}
struct Turn {
    control: StreamingViewerControl,
    ready: Rc<Cell<bool>>,
    completed: Rc<Cell<bool>>,
    dropped: Rc<Cell<Option<bool>>>,
    requires_fence: Rc<Cell<bool>>,
    expire: Option<Rc<Cell<bool>>>,
    result: Result<(), Error>,
}
impl Future for Turn {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.completed.get(), "completed network turn polled again");
        if self.requires_fence.get() {
            assert!(
                self.control.is_stopped(),
                "I/O polled before authority fence"
            );
        }
        if !self.ready.get() {
            return Poll::Pending;
        }
        if let Some(live) = &self.expire {
            live.set(false);
        }
        self.completed.set(true);
        Poll::Ready(self.result)
    }
}
impl Drop for Turn {
    fn drop(&mut self) {
        self.dropped.set(Some(self.control.is_stopped()));
    }
}
fn turn(control: &StreamingViewerControl, ready: bool) -> Turn {
    Turn {
        control: control.clone(),
        ready: Rc::new(Cell::new(ready)),
        completed: Rc::new(Cell::new(false)),
        dropped: Rc::new(Cell::new(None)),
        requires_fence: Rc::new(Cell::new(false)),
        expire: None,
        result: Ok(()),
    }
}

#[test]
fn recovery_decoder_failure_after_pending_io_fences_drains_and_keeps_its_cause() {
    let (_runtime, control) = control();
    let mut native = native(&control, false);
    native.result = Err(decoder_startup::Error::UnsupportedConfiguration);
    let ready_native = native.ready.clone();
    let polls = native.polls.clone();
    let mut native = Box::pin(native);
    let mut network = turn(&control, false);
    network.result = Err(Error::Closed);
    let ready = network.ready.clone();
    let complete = network.completed.clone();
    let dropped = network.dropped.clone();
    let fence = network.requires_fence.clone();
    let live = Cell::new(true);
    let mut joined = Box::pin(checked(
        native.as_mut(),
        network,
        &control,
        Error::Startup,
        || permission(&live, &control),
    ));
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    ready_native.set(true);
    fence.set(true);
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    assert!(control.is_stopped());
    assert_eq!(dropped.get(), None);
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    assert_eq!(polls.get(), 2);
    ready.set(true);
    assert_eq!(
        poll(joined.as_mut()),
        Poll::Ready(Err(Error::Startup(
            decoder_startup::Error::UnsupportedConfiguration
        )))
    );
    assert!(complete.get());
    drop(joined);
    assert_eq!(dropped.get(), Some(true));
}

#[test]
fn expiry_beats_a_late_native_success_without_abandoning_pending_io() {
    let (_runtime, control) = control();
    let mut native = Box::pin(native(&control, false));
    let ready_native = native.ready.clone();
    let polls = native.polls.clone();
    let native_dropped = native.dropped.clone();
    let mut network = turn(&control, false);
    network.result = Err(Error::Closed);
    let ready = network.ready.clone();
    let complete = network.completed.clone();
    let dropped = network.dropped.clone();
    let fence = network.requires_fence.clone();
    let live = Cell::new(true);
    let mut joined = Box::pin(checked(
        native.as_mut(),
        network,
        &control,
        Error::Startup,
        || permission(&live, &control),
    ));
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    live.set(false);
    ready_native.set(true);
    fence.set(true);
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    assert_eq!(polls.get(), 1, "expired native work was polled again");
    assert_eq!(dropped.get(), None);
    assert_eq!(native_dropped.get(), None);
    // Cancellation's Closed cannot overwrite the original Expired cause.
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    ready.set(true);
    assert_eq!(poll(joined.as_mut()), Poll::Ready(Err(expired())));
    assert!(complete.get());
    drop(joined);
    drop(native);
    assert_eq!(native_dropped.get(), Some(true));
}

#[test]
fn retained_native_completion_does_not_extend_the_recovery_budget() {
    let (_runtime, control) = control();
    let mut native = Box::pin(native(&control, true));
    let polls = native.polls.clone();
    let network = turn(&control, false);
    let ready = network.ready.clone();
    let live = Cell::new(true);
    let mut joined = Box::pin(checked(
        native.as_mut(),
        network,
        &control,
        Error::Startup,
        || permission(&live, &control),
    ));
    assert_eq!(poll(joined.as_mut()), Poll::Pending);
    live.set(false);
    ready.set(true);
    assert_eq!(poll(joined.as_mut()), Poll::Ready(Err(expired())));
    assert_eq!(polls.get(), 1);
    assert!(control.is_stopped());
}

#[test]
fn expiry_inside_a_successful_network_poll_is_checked_before_handoff() {
    let (_runtime, control) = control();
    let mut native = Box::pin(native(&control, true));
    let live = Rc::new(Cell::new(true));
    let mut network = turn(&control, true);
    network.expire = Some(live.clone());
    let mut joined = Box::pin(checked(
        native.as_mut(),
        network,
        &control,
        Error::Startup,
        || permission(&live, &control),
    ));
    assert_eq!(poll(joined.as_mut()), Poll::Ready(Err(expired())));
    assert!(control.is_stopped());
}

#[test]
fn expired_unpolled_recovery_does_no_native_work() {
    let (_runtime, control) = control();
    let mut native = Box::pin(native(&control, true));
    let polls = native.polls.clone();
    let network = turn(&control, true);
    network.requires_fence.set(true);
    let live = Cell::new(false);
    let mut joined = Box::pin(checked(
        native.as_mut(),
        network,
        &control,
        Error::Startup,
        || permission(&live, &control),
    ));
    assert_eq!(poll(joined.as_mut()), Poll::Ready(Err(expired())));
    assert_eq!(polls.get(), 0);
}

#[test]
fn a_named_network_failure_fences_pending_recovery_without_relabeling_it() {
    let (_runtime, control) = control();
    let mut native = Box::pin(native(&control, false));
    let dropped = native.dropped.clone();
    let mut network = turn(&control, true);
    network.result = Err(Error::UnclaimedRecord(0x0054));
    let live = Cell::new(true);
    let mut joined = Box::pin(checked(
        native.as_mut(),
        network,
        &control,
        Error::Startup,
        || permission(&live, &control),
    ));
    assert_eq!(
        poll(joined.as_mut()),
        Poll::Ready(Err(Error::UnclaimedRecord(0x0054)))
    );
    drop(joined);
    drop(native);
    assert_eq!(dropped.get(), Some(true));
}

#[test]
fn recovery_distinguishes_pending_decode_from_completed_absence_and_a_frame() {
    let (_runtime, control) = control();
    for value in [None, Some(17)] {
        let mut native = Box::pin(std::future::ready(Ok::<_, decoder_startup::Error>(value)));
        let live = Cell::new(true);
        let mut joined = Box::pin(checked(
            native.as_mut(),
            turn(&control, true),
            &control,
            Error::Startup,
            || permission(&live, &control),
        ));
        assert_eq!(poll(joined.as_mut()), Poll::Ready(Ok(Some(value))));
        assert!(!control.is_stopped());
    }
}
