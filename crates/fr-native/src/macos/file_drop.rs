//! Explicit native selections on the original controlled upload intake.
//!
//! AppKit submits bounded URL metadata only after a drop or local button
//! gesture. The shared desktop worker pins descriptors off the window thread;
//! the original frd owner alone admits contents and produces receipts.
#![forbid(unsafe_code)]

use crate::{
    file_drop::{self, Inbox},
    file_selection::{BatchStatus, Control, Error, Manager, Surface},
};
use frd::native_files::{SendControl, SendIntake, SendRequest, SendSnapshot};
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Default)]
struct State {
    inbox: Inbox,
    stopped: AtomicBool,
    eligible: AtomicBool,
    latest: Mutex<Option<SendControl>>,
}

/// Native callbacks borrow this original owner without reentering the session
/// or borrowing its main-thread RefCells.
#[derive(Clone, Default)]
pub(crate) struct Ingress(Arc<State>);
impl Ingress {
    pub(crate) fn ready(&self) -> bool {
        self.live() && self.0.eligible.load(Ordering::Acquire) && self.0.inbox.ready()
    }

    pub(crate) fn submit(&self, bytes: &[u8], captured: Instant) -> bool {
        if !self.ready() {
            return false;
        }
        let Ok(paths) = file_drop::paths(bytes) else {
            return false;
        };
        // Inbox repeats the negative checks under its own lock. A stop racing
        // parsing cannot retain this intent for another attempt.
        self.0.inbox.publish(paths, captured)
    }

    pub(crate) fn set_ready(&self, ready: bool) {
        self.0.eligible.store(ready, Ordering::Release);
        if !ready {
            self.0.inbox.set_ready(false);
        }
    }

    pub(crate) fn stop(&self) {
        // Serialize the complete intake admission/handle-publication operation
        // with local cancellation. This lock never covers filesystem/native IO.
        let latest = self
            .0
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.0.stopped.store(true, Ordering::Release);
        self.0.eligible.store(false, Ordering::Release);
        self.0.inbox.close();
        if let Some(control) = &*latest {
            control.cancel();
        }
    }
}
impl Surface for Ingress {
    fn live(&self) -> bool {
        !self.0.stopped.load(Ordering::Acquire) && !self.0.inbox.is_closed()
    }
    fn ready(&self) -> bool {
        self.live() && self.0.eligible.load(Ordering::Acquire)
    }
    fn submit(
        &self,
        intake: &SendIntake,
        request: SendRequest,
        captured: Instant,
    ) -> Result<(SendControl, bool), &'static str> {
        let mut latest = self
            .0
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if captured.elapsed() >= file_drop::MAX_AGE || !Surface::ready(self) {
            return Err("file_selection_expired");
        }
        let source = intake
            .submit(request)
            .map_err(|_| "file_selection_unavailable")?;
        *latest = Some(source.clone());
        Ok((source, true))
    }
}

#[derive(Default)]
pub(crate) struct Owner {
    ingress: Ingress,
    configured: bool,
    manager: Option<Manager>,
    control: Option<Control>,
}
impl fmt::Debug for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeFileSelections")
            .field("configured", &self.configured)
            .field("live", &self.ingress.live())
            .field("control", &self.control)
            .finish()
    }
}
impl Owner {
    pub(crate) fn ingress(&self) -> Ingress {
        self.ingress.clone()
    }
    pub(crate) fn configure(&mut self, intake: SendIntake) -> Result<(), Error> {
        if self.configured {
            return Err(Error::AlreadyEnabled);
        }
        self.configured = true;
        let manager =
            Manager::start_on(self.ingress.0.inbox.clone(), intake, self.ingress.clone())?;
        self.control = Some(manager.control());
        self.manager = Some(manager);
        Ok(())
    }
    pub(crate) fn cancel(&self) {
        self.ingress.stop();
        if let Some(manager) = &self.manager {
            manager.stop();
        }
    }
    pub(crate) fn complete(&self) -> bool {
        self.manager.as_ref().is_none_or(Manager::is_finished)
    }
    pub(crate) fn batches(&self) -> Vec<BatchStatus> {
        self.control
            .as_ref()
            .map_or_else(Vec::new, Control::batches)
    }
    pub(crate) fn progress(&self) -> Option<Result<SendSnapshot, &'static str>> {
        self.control.as_ref()?.latest_snapshot()
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn native_ingress_keeps_one_local_selection_and_never_reopens_after_stop() {
        let ingress = Ingress::default();
        let uri = b"file:///tmp/not-opened-at-the-native-boundary";
        assert!(!ingress.submit(uri, Instant::now()));
        ingress.set_ready(true);
        ingress.0.inbox.set_ready(true);
        for invalid in [
            b"https://example.com/private".as_slice(),
            b"file://remote-host/private",
            b"file:///tmp/one%2Ftwo",
            b"file:///tmp/one\nfile:///tmp/one",
        ] {
            assert!(!ingress.submit(invalid, Instant::now()));
            assert!(ingress.ready());
        }
        assert!(!ingress.submit(uri, Instant::now() - Duration::from_secs(3)));
        assert!(ingress.submit(uri, Instant::now()));
        assert!(!ingress.submit(b"file:///tmp/replacement", Instant::now()));
        ingress.stop();
        ingress.set_ready(true);
        ingress.0.inbox.set_ready(true);
        assert!(!ingress.ready());
        assert!(ingress.0.inbox.take().is_none());
        let retired = ingress.0.inbox.take_retired().unwrap();
        assert_eq!(
            (retired.count, retired.reason),
            (1, "file_selection_stopped")
        );
        assert!(!ingress.submit(uri, Instant::now()));
    }
}
