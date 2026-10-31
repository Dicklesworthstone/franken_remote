use super::*;
use std::{
    cell::{Cell, RefCell},
    pin::pin,
    rc::Rc,
    task::{Context, Waker},
};

fn waker() -> Waker {
    Waker::noop().clone()
}

struct Probe {
    index: usize,
    ready: Rc<Vec<Cell<bool>>>,
    dropped: Rc<RefCell<Vec<usize>>>,
}
impl Future for Probe {
    type Output = usize;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<usize> {
        if self.ready[self.index].get() {
            Poll::Ready(self.index)
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.borrow_mut().push(self.index);
    }
}

#[test]
fn a_stalled_peer_does_not_block_other_slots_and_output_keeps_candidate_order() {
    let ready = Rc::new((0..7).map(|_| Cell::new(false)).collect::<Vec<_>>());
    let dropped = Rc::new(RefCell::new(Vec::new()));
    let started = RefCell::new(Vec::new());
    let future = probe_batch(
        7,
        |index| {
            started.borrow_mut().push(index);
            Probe {
                index,
                ready: ready.clone(),
                dropped: dropped.clone(),
            }
        },
        || true,
    )
    .unwrap();
    let mut future = pin!(future);
    let waker = waker();
    let mut task = Context::from_waker(&waker);
    assert!(future.as_mut().poll(&mut task).is_pending());
    assert_eq!(*started.borrow(), [0, 1, 2, 3]);
    ready[2].set(true);
    assert!(future.as_mut().poll(&mut task).is_pending());
    assert!(future.as_mut().poll(&mut task).is_pending());
    assert_eq!(*started.borrow(), [0, 1, 2, 3, 4]);
    assert_eq!(*dropped.borrow(), [2]);
    for value in ready.iter() {
        value.set(true);
    }
    let result = loop {
        if let Poll::Ready(result) = future.as_mut().poll(&mut task) {
            break result;
        }
    };
    assert_eq!(
        result,
        (0..7).map(ProbeOutcome::Completed).collect::<Vec<_>>()
    );
    assert_eq!(dropped.borrow().len(), 7);
}

#[test]
fn cancellation_drops_exact_live_probes_and_keeps_completed_evidence() {
    let ready = Rc::new(
        (0..9)
            .map(|index| Cell::new(index == 1))
            .collect::<Vec<_>>(),
    );
    let dropped = Rc::new(RefCell::new(Vec::new()));
    let permitted = Cell::new(true);
    let future = probe_batch(
        9,
        |index| Probe {
            index,
            ready: ready.clone(),
            dropped: dropped.clone(),
        },
        || permitted.get(),
    )
    .unwrap();
    let mut future = pin!(future);
    let waker = waker();
    let mut task = Context::from_waker(&waker);
    assert!(future.as_mut().poll(&mut task).is_pending());
    permitted.set(false);
    let Poll::Ready(result) = future.as_mut().poll(&mut task) else {
        panic!("cancelled batch");
    };
    assert_eq!(result[1], ProbeOutcome::Completed(1));
    for i in [0, 2, 3] {
        assert_eq!(result[i], ProbeOutcome::Interrupted);
    }
    assert!(result[4..].iter().all(|r| *r == ProbeOutcome::NotStarted));
    dropped.borrow_mut().sort_unstable();
    assert_eq!(*dropped.borrow(), [0, 1, 2, 3]);
}

#[test]
fn dropping_the_batch_drops_its_original_children_without_starting_more() {
    let ready = Rc::new((0..8).map(|_| Cell::new(false)).collect::<Vec<_>>());
    let dropped = Rc::new(RefCell::new(Vec::new()));
    let future = probe_batch(
        8,
        |index| Probe {
            index,
            ready: ready.clone(),
            dropped: dropped.clone(),
        },
        || true,
    )
    .unwrap();
    let mut future = Box::pin(future);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(&waker()))
            .is_pending()
    );
    drop(future);
    dropped.borrow_mut().sort_unstable();
    assert_eq!(*dropped.borrow(), [0, 1, 2, 3]);
}

#[test]
fn immediately_completed_peers_are_still_cooperative_and_admission_is_bounded() {
    let started = Cell::new(0);
    let future = probe_batch(
        MAX_BATCH_TARGETS,
        |index| {
            started.set(started.get() + 1);
            std::future::ready(index)
        },
        || true,
    )
    .unwrap();
    let mut future = pin!(future);
    let waker = waker();
    let mut task = Context::from_waker(&waker);
    for turn in 1..=16 {
        let result = future.as_mut().poll(&mut task);
        assert_eq!(started.get(), turn * MAX_BATCH_CONCURRENCY);
        assert_eq!(result.is_ready(), turn == 16);
    }
    assert!(probe_batch(MAX_BATCH_TARGETS + 1, |_| std::future::ready(()), || true).is_err());
}

#[test]
fn a_lapsed_sweep_never_constructs_a_probe_and_empty_sweeps_complete() {
    for (count, permitted) in [(8, false), (0, true)] {
        let started = Cell::new(0);
        let future = probe_batch(
            count,
            |_| {
                started.set(started.get() + 1);
                std::future::ready(())
            },
            || permitted,
        )
        .unwrap();
        let mut future = pin!(future);
        let Poll::Ready(result) = future.as_mut().poll(&mut Context::from_waker(&waker())) else {
            panic!("complete");
        };
        assert_eq!(result.len(), count);
        assert!(result.iter().all(|r| *r == ProbeOutcome::NotStarted));
        assert_eq!(started.get(), 0);
    }
}

#[test]
fn expiry_during_construction_drops_that_probe_before_its_first_poll() {
    let permitted = Cell::new(true);
    let polls = Cell::new(0);
    let future = probe_batch(
        2,
        |_| {
            permitted.set(false);
            poll_fn(|_| {
                polls.set(polls.get() + 1);
                Poll::Ready(())
            })
        },
        || permitted.get(),
    )
    .unwrap();
    let mut future = pin!(future);
    let Poll::Ready(result) = future.as_mut().poll(&mut Context::from_waker(&waker())) else {
        panic!("expired");
    };
    assert_eq!(polls.get(), 0);
    assert_eq!(
        result,
        [ProbeOutcome::Interrupted, ProbeOutcome::NotStarted]
    );
}

#[test]
fn completion_that_crosses_the_original_deadline_cannot_publish_a_success() {
    let permitted = Cell::new(true);
    let future = probe_batch(
        1,
        |_| {
            poll_fn(|_| {
                permitted.set(false);
                Poll::Ready("late advertisement")
            })
        },
        || permitted.get(),
    )
    .unwrap();
    let mut future = pin!(future);
    let Poll::Ready(result) = future.as_mut().poll(&mut Context::from_waker(&waker())) else {
        panic!("expired");
    };
    assert_eq!(result, [ProbeOutcome::Interrupted]);
}
