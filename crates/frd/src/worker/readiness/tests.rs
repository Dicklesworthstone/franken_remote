use super::*;
use std::{future::poll_fn, pin::pin, sync::atomic::AtomicUsize};

#[derive(Default)]
struct Counter(AtomicUsize);
impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
fn counter() -> (Arc<Counter>, Waker) {
    let count = Arc::new(Counter::default());
    let waker = Waker::from(Arc::clone(&count));
    (count, waker)
}

#[test]
fn unrelated_network_and_watchdog_turns_do_not_poll_the_pipe() {
    let gate = Readiness::new();
    let polls = AtomicUsize::new(0);
    let saved = Mutex::new(None);
    let mut io = pin!(poll_fn(|cx| {
        polls.fetch_add(1, Ordering::Relaxed);
        *saved.lock().unwrap() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    let (count, parent) = counter();
    let mut cx = Context::from_waker(&parent);
    for _ in 0..10_000 {
        assert!(gate.poll(io.as_mut(), &mut cx).is_pending());
    }
    assert_eq!(polls.load(Ordering::Relaxed), 1);
    assert_eq!(count.0.load(Ordering::Relaxed), 0);
    saved.lock().unwrap().take().unwrap().wake();
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
    assert!(gate.poll(io.as_mut(), &mut cx).is_pending());
    assert_eq!(polls.load(Ordering::Relaxed), 2);
}

#[test]
fn a_wake_during_poll_is_not_lost() {
    let gate = Readiness::new();
    let mut first = true;
    let mut io = pin!(poll_fn(|cx| {
        if first {
            first = false;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(17)
        }
    }));
    let (count, parent) = counter();
    let mut cx = Context::from_waker(&parent);
    assert!(gate.poll(io.as_mut(), &mut cx).is_pending());
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
    assert_eq!(gate.poll(io.as_mut(), &mut cx), Poll::Ready(17));
}

#[test]
fn pending_io_wakes_the_new_parent_after_a_network_turn() {
    let gate = Readiness::new();
    let saved = Mutex::new(None);
    let mut io = pin!(poll_fn(|cx| {
        *saved.lock().unwrap() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    let (old_count, old_parent) = counter();
    let (new_count, new_parent) = counter();
    assert!(
        gate.poll(io.as_mut(), &mut Context::from_waker(&old_parent))
            .is_pending()
    );
    assert!(
        gate.poll(io.as_mut(), &mut Context::from_waker(&new_parent))
            .is_pending()
    );
    saved.lock().unwrap().take().unwrap().wake();
    assert_eq!(old_count.0.load(Ordering::Relaxed), 0);
    assert_eq!(new_count.0.load(Ordering::Relaxed), 1);
    // Replacing a parent releases its task even while the I/O stays pending.
    assert_eq!(Arc::strong_count(&old_count), 2);
}

#[test]
fn dropping_the_operation_detaches_a_retained_reactor_waker() {
    let gate = Readiness::new();
    let saved = Mutex::new(None);
    let mut io = pin!(poll_fn(|cx| {
        *saved.lock().unwrap() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    let (count, parent) = counter();
    assert!(
        gate.poll(io.as_mut(), &mut Context::from_waker(&parent))
            .is_pending()
    );
    let reactor = saved.lock().unwrap().take().unwrap();
    drop(gate);
    reactor.wake_by_ref();
    assert_eq!(count.0.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&count), 2);
}

#[test]
fn wakes_on_another_thread_coalesce_without_losing_completion() {
    let gate = Readiness::new();
    let completed = AtomicBool::new(false);
    let saved = Mutex::new(None);
    let mut io = pin!(poll_fn(|cx| {
        *saved.lock().unwrap() = Some(cx.waker().clone());
        if completed.load(Ordering::Acquire) {
            Poll::Ready(42)
        } else {
            Poll::Pending
        }
    }));
    let (_, parent) = counter();
    let mut cx = Context::from_waker(&parent);
    assert!(gate.poll(io.as_mut(), &mut cx).is_pending());
    let reactor = saved.lock().unwrap().take().unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                completed.store(true, Ordering::Release);
                for _ in 0..1000 {
                    reactor.wake_by_ref();
                }
            })
            .join()
            .unwrap();
    });
    assert_eq!(gate.poll(io.as_mut(), &mut cx), Poll::Ready(42));
}
