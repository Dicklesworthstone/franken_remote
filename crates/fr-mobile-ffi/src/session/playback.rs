//! Playback supplied by a native shell, retained by its original session.
use asupersync::{cx::Cx, time::sleep};
use frd::{session_startup::NativeObserver, worker::Deadline};
use std::time::Duration;

/// One original local playback owner. All methods are bounded and nonblocking.
/// Stopping fences its output immediately. Positive completion includes native
/// device disposal and the decoder worker's actual exit. Drop also fences output.
pub trait Playback {
    fn stop(&self);
    fn is_complete(&self) -> bool;
    /// Publish only content-free native status to the shell.
    fn poll(&self);
}

/// The shell's explicit request follows observing and controlling attempts.
/// Native construction happens on the session thread after host admission.
/// A factory must retain cleanup even when the host omits the audio capability.
pub type PlaybackFactory = Box<dyn FnMut(&mut NativeObserver) -> Box<dyn Playback> + Send>;

pub(super) fn poll(owner: Option<&dyn Playback>) {
    if let Some(owner) = owner {
        owner.poll();
    }
}

pub(super) fn pending(owner: Option<&dyn Playback>) -> bool {
    poll(owner);
    owner.is_some_and(|owner| !owner.is_complete())
}

pub(super) async fn reap(owner: Option<&dyn Playback>, cx: &Cx, deadline: Deadline) -> bool {
    while pending(owner) {
        let Some(timer) = cx.timer_driver() else {
            return false;
        };
        let now = timer.now();
        if cx.checkpoint().is_err() || now >= deadline.time() {
            return false;
        }
        let remaining = Duration::from_nanos(deadline.time().as_nanos() - now.as_nanos());
        sleep(now, Duration::from_millis(10).min(remaining)).await;
    }
    true
}
