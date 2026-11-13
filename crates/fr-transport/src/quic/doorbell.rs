//! A wake for one connection's network turn. A turn waits for a packet or its
//! timeout; records that become ready for this connection meanwhile (a
//! picture published by the capture task) would otherwise wait out the rest
//! of that timeout. Ringing ends the current wait at once, or the next one if
//! none is under way.
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

/// Shared by a connection and whoever produces its records. Holds no record,
/// connection or authority: ringing only shortens one wait.
#[derive(Clone, Default)]
pub struct Doorbell(Arc<Mutex<Bell>>);

#[derive(Default)]
struct Bell {
    /// Rung since the last wait ended.
    rung: bool,
    /// The turn waiting now, if one is.
    waiting: Option<Waker>,
}

impl std::fmt::Debug for Doorbell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Doorbell")
    }
}

impl Doorbell {
    /// Ends the connection's current wait, or its next one. Rings before that
    /// wait coalesce into one.
    pub fn ring(&self) {
        let waiting = self.0.lock().ok().and_then(|mut bell| {
            bell.rung = true;
            bell.waiting.take()
        });
        if let Some(waker) = waiting {
            waker.wake();
        }
    }
    fn poll_rung(&self, task: &mut Context<'_>) -> Poll<()> {
        // A poisoned bell cannot hold a turn back: the wait ends as if rung.
        let Ok(mut bell) = self.0.lock() else {
            return Poll::Ready(());
        };
        if std::mem::take(&mut bell.rung) {
            bell.waiting = None;
            return Poll::Ready(());
        }
        match &mut bell.waiting {
            Some(waker) if waker.will_wake(task.waker()) => {}
            waiting => *waiting = Some(task.waker().clone()),
        }
        Poll::Pending
    }
    fn stop_waiting(&self) {
        if let Ok(mut bell) = self.0.lock() {
            bell.waiting = None;
        }
    }
}

/// One wait's view of the doorbell: ready when it rings.
pub(super) struct Rung<'a>(pub(super) &'a Doorbell);

impl Future for Rung<'_> {
    type Output = ();
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<()> {
        self.0.poll_rung(task)
    }
}

impl Drop for Rung<'_> {
    fn drop(&mut self) {
        // A wait that ended another way leaves no waker behind for a later
        // ring to wake spuriously.
        self.0.stop_waiting();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll(rung: &mut Rung<'_>, waker: &Waker) -> Poll<()> {
        Pin::new(rung).poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn a_ring_during_a_wait_wakes_it_once_and_ends_it() {
        let bell = Doorbell::default();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut rung = Rung(&bell);
        assert_eq!(poll(&mut rung, &waker), Poll::Pending);
        assert_eq!(poll(&mut rung, &waker), Poll::Pending);
        bell.ring();
        bell.ring();
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert_eq!(poll(&mut rung, &waker), Poll::Ready(()));
        // Both rings were consumed by that one wait.
        let mut next = Rung(&bell);
        assert_eq!(poll(&mut next, &waker), Poll::Pending);
    }

    #[test]
    fn a_ring_between_waits_ends_the_next_wait_at_once() {
        let bell = Doorbell::default();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        bell.ring();
        let mut rung = Rung(&bell);
        assert_eq!(poll(&mut rung, &waker), Poll::Ready(()));
        assert_eq!(count.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_ended_wait_leaves_no_waker_behind() {
        let bell = Doorbell::default();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut rung = Rung(&bell);
        assert_eq!(poll(&mut rung, &waker), Poll::Pending);
        drop(rung);
        bell.ring();
        assert_eq!(count.0.load(Ordering::SeqCst), 0, "no spurious wake");
    }
}
