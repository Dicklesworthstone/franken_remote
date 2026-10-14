//! One persistent wait for the handled termination signals.
use asupersync::signal::Signal;
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
};

pub(crate) type Signals = (Option<Signal>, Option<Signal>);

/// Resolves once one of `signals` is delivered. `Signal::recv` registers its
/// waker in a future that must stay alive between polls: building a fresh
/// `recv()` on every poll and dropping it unregistered the waker, so a task
/// with no other wakeups (an idle `frd ingress-helper`) never saw SIGTERM or
/// SIGINT, and `frd run` noticed one only at its next unrelated wakeup.
pub(crate) async fn first(signals: &mut Signals) {
    let (a, b) = signals;
    let mut a = pin!(delivered(a.as_mut()));
    let mut b = pin!(delivered(b.as_mut()));
    poll_fn(|task| {
        if a.as_mut().poll(task).is_ready() || b.as_mut().poll(task).is_ready() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}
async fn delivered(signal: Option<&mut Signal>) {
    if let Some(signal) = signal
        && signal.recv().await.is_some()
    {
        return;
    }
    std::future::pending::<()>().await;
}

/// `first`, kept alive across several sequential waits; fired stays fired.
pub(crate) struct Watch<'a>(Option<Pin<Box<dyn Future<Output = ()> + 'a>>>);
impl<'a> Watch<'a> {
    pub(crate) fn new(signals: &'a mut Signals) -> Self {
        Self(Some(Box::pin(first(signals))))
    }
    pub(crate) fn fired(&mut self, task: &mut Context<'_>) -> bool {
        if let Some(wait) = &mut self.0 {
            if wait.as_mut().poll(task).is_pending() {
                return false;
            }
            self.0 = None;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{
        runtime::RuntimeBuilder,
        signal::{SignalKind, signal},
    };
    use std::time::{Duration, Instant};

    /// An otherwise idle task is woken by the signal itself. SIGUSR2 is used
    /// because nothing else in frd handles it.
    #[test]
    fn an_idle_wait_wakes_on_the_signal_itself() {
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let mut signals: Signals = (signal(SignalKind::user_defined2()).ok(), None);
        assert!(signals.0.is_some());
        let pid = std::process::id().to_string();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                std::process::Command::new("kill")
                    .args(["-USR2", &pid])
                    .status()
                    .unwrap()
                    .success()
            );
        });
        let started = Instant::now();
        let mut watch = Watch::new(&mut signals);
        // The timer bounds a broken wait: it would complete only when the
        // timeout itself polled it again, after 5 s.
        let woken = runtime.block_on(async {
            let cx = asupersync::cx::Cx::current().unwrap();
            asupersync::time::timeout(
                cx.now(),
                Duration::from_secs(5),
                poll_fn(|task| {
                    if watch.fired(task) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                }),
            )
            .await
        });
        sender.join().unwrap();
        assert!(woken.is_ok());
        // Woken by the delivery, not by some later unrelated poll.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
