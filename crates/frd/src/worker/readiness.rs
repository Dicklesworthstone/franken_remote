//! Separate a pending worker operation's wakeups from its supervising session.
//! Network turns and the cancellation watchdog must not repoll an idle pipe.
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

pub(super) struct Readiness {
    signal: Arc<Signal>,
    waker: Waker,
}
struct Signal {
    ready: AtomicBool,
    // Arc cloning under the lock cannot invoke an arbitrary executor callback.
    parent: Mutex<Option<Arc<Waker>>>,
}
impl Signal {
    fn parent(&self) -> MutexGuard<'_, Option<Arc<Waker>>> {
        self.parent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.ready.store(true, Ordering::Release);
        let parent = self.parent().clone();
        // Never invoke the parent waker with the registration lock held.
        if let Some(parent) = parent {
            parent.wake_by_ref();
        }
    }
}
impl Readiness {
    pub(super) fn new() -> Self {
        let signal = Arc::new(Signal {
            ready: AtomicBool::new(true),
            parent: Mutex::new(None),
        });
        Self {
            waker: Waker::from(Arc::clone(&signal)),
            signal,
        }
    }
    pub(super) fn poll<F: Future + ?Sized>(
        &self,
        future: Pin<&mut F>,
        task: &mut Context<'_>,
    ) -> Poll<F::Output> {
        // A select/composition may supply a different parent on the next turn.
        let replace = self
            .signal
            .parent()
            .as_ref()
            .is_none_or(|parent| !parent.will_wake(task.waker()));
        if replace {
            let parent = Arc::new(task.waker().clone());
            let old = self.signal.parent().replace(parent);
            drop(old);
        }
        // Register BEFORE consuming readiness. A wake during polling remains
        // set for the next turn, including one racing with Poll::Pending.
        if !self.signal.ready.swap(false, Ordering::AcqRel) {
            return Poll::Pending;
        }
        future.poll(&mut Context::from_waker(&self.waker))
    }
}
impl Drop for Readiness {
    fn drop(&mut self) {
        // A reactor may retain the inner waker after completion/cancellation.
        // It must not retain the session task or keep waking a retired owner.
        let old = self.signal.parent().take();
        drop(old);
    }
}

#[cfg(test)]
#[path = "readiness/tests.rs"]
mod tests;
