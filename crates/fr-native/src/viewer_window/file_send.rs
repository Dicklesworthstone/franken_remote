//! Repeated explicit local selections for the original desktop upload lane.
//!
//! Filesystem metadata/open calls run on one owned worker, never on the window
//! or authority thread. Its descriptors, result handles and bounded history
//! belong to this attempt only; reconnect must join this worker before opening
//! another desktop. The original sender remains the only source/transport owner.
#![forbid(unsafe_code)]

use super::{
    Status as WindowStatus, WindowControl,
    file_drop::{Dropped, Inbox, MAX_AGE},
};
use crate::file_progress::{Control as PanelControl, Status as PanelStatus};
use frd::native_files::{Selection, SendControl, SendIntake, SendStatus};
use std::{
    fmt,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const TURN: Duration = Duration::from_millis(10);
/// Bound content-free history as well as explicitly requested selections.
/// The host's cumulative transfer/byte limits still apply across every batch.
const MAX_BATCHES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    AlreadyEnabled,
    ThreadUnavailable,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Original-owner outcomes in selection order; never a path or filename.
/// Unknown directory sizes remain unknown until the sender measures them.
#[derive(Debug, Clone)]
pub struct BatchStatus {
    pub sizes: Vec<Option<u64>>,
    pub status: Result<SendStatus, &'static str>,
}
enum Outcome {
    Preparing,
    Refused(&'static str),
    Submitted(SendControl),
}
struct Batch {
    sizes: Vec<Option<u64>>,
    outcome: Outcome,
}
#[derive(Default)]
struct Shared {
    stopped: AtomicBool,
    batches: Mutex<Vec<Batch>>,
}
impl Shared {
    fn batches(&self) -> MutexGuard<'_, Vec<Batch>> {
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    fn record_retired(&self, inbox: &Inbox) {
        if let Some(retired) = inbox.take_retired() {
            let mut batches = self.batches();
            // Readiness reserves capacity before accepting the one inbox slot.
            // A retired intent consumes that same bounded history entry.
            if batches.len() < MAX_BATCHES {
                batches.push(Batch {
                    sizes: vec![None; retired.count],
                    outcome: Outcome::Refused(retired.reason),
                });
            }
        }
    }
}
/// Read-only, content-free receipts for additional selections on one desktop.
/// Keeping this handle neither keeps the session live nor enables another drop.
#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl Control {
    pub fn batches(&self) -> Vec<BatchStatus> {
        self.0
            .batches()
            .iter()
            .map(|batch| BatchStatus {
                sizes: batch.sizes.clone(),
                status: match &batch.outcome {
                    Outcome::Preparing => Err("file_selection_preparing"),
                    Outcome::Refused(code) => Err(*code),
                    Outcome::Submitted(control) => Ok(control.status()),
                },
            })
            .collect()
    }
}
impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileDropControl")
            .field("batches", &self.0.batches().len())
            .field("stopped", &self.0.stopped())
            .finish()
    }
}

/// This owner is retained beside the original window through native cleanup.
/// Stop is immediate; finish never blocks on a foreign filesystem operation.
pub(crate) struct Manager {
    shared: Arc<Shared>,
    inbox: Inbox,
    thread: Option<JoinHandle<()>>,
}
impl Manager {
    pub(crate) fn start(
        window: WindowControl,
        intake: SendIntake,
        panel: PanelControl,
    ) -> Result<Self, Error> {
        if window.status() != WindowStatus::Mapped
            || intake.is_closed()
            || matches!(panel.status(), PanelStatus::Stopped(_))
        {
            return Err(Error::Unavailable);
        }
        let shared = Arc::new(Shared::default());
        let inbox = window.file_drop_inbox();
        if inbox.is_closed() {
            return Err(Error::Unavailable);
        }
        let worker = Worker {
            shared: Arc::clone(&shared),
            inbox: inbox.clone(),
            window,
            intake,
            panel,
        };
        let thread = thread::Builder::new()
            .name("fr-file-selection".into())
            .spawn(move || worker.run())
            .map_err(|_| Error::ThreadUnavailable)?;
        Ok(Self {
            shared,
            inbox,
            thread: Some(thread),
        })
    }
    pub(crate) fn control(&self) -> Control {
        Control(Arc::clone(&self.shared))
    }
    pub(crate) fn stop(&self) {
        self.shared.stop();
        self.inbox.close();
    }
    pub(crate) fn finish(&mut self) -> bool {
        let Some(thread) = &self.thread else {
            return true;
        };
        if !thread.is_finished() {
            return false;
        }
        // Completion, including unwind, proves this sole selection worker no
        // longer retains paths/descriptors. It says nothing about remote effects.
        let _joined = self.thread.take().expect("original worker").join();
        true
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Worker {
    shared: Arc<Shared>,
    inbox: Inbox,
    window: WindowControl,
    intake: SendIntake,
    panel: PanelControl,
}
impl Worker {
    fn live(&self) -> bool {
        !self.shared.stopped()
            && !self.inbox.is_closed()
            && self.window.status() == WindowStatus::Mapped
            && !self.intake.is_closed()
            && !matches!(self.panel.status(), PanelStatus::Stopped(_))
    }
    fn ready(&self) -> bool {
        self.live()
            && self.intake.ready()
            && self.panel.status() == PanelStatus::Running
            && self.shared.batches().len() < MAX_BATCHES
    }
    fn run(self) {
        while self.live() {
            // Collect an already admitted intent before changing readiness.
            // Preparation rechecks the original lane; a stale/closed selection
            // gets its own result instead of being silently replaced.
            if let Some(dropped) = self.inbox.take() {
                self.prepare(dropped);
            }
            self.shared.record_retired(&self.inbox);
            // Eligibility comes from the original lane and panel, NOT from
            // Inbox::ready (which is false while a valid drop waits in its slot).
            self.inbox.set_ready(self.ready());
            thread::sleep(TURN);
        }
    }
    fn prepare(&self, dropped: Dropped) {
        let index = {
            let mut batches = self.shared.batches();
            if batches.len() >= MAX_BATCHES {
                return;
            }
            let index = batches.len();
            batches.push(Batch {
                sizes: vec![None; dropped.paths.len()],
                outcome: Outcome::Preparing,
            });
            index
        };
        let outcome = self.select(&dropped, index);
        self.shared.batches()[index].outcome = outcome;
    }
    fn select(&self, dropped: &Dropped, index: usize) -> Outcome {
        if !self.ready_for(dropped) {
            return Outcome::Refused("file_selection_expired");
        }
        // This is the sole potentially blocking local path operation. The
        // native window and original session continue on their own owners.
        let selected = match Selection::select(&dropped.paths) {
            Ok(selected) => selected,
            Err(refusal) => return Outcome::Refused(refusal.reason.code()),
        };
        self.shared.batches()[index].sizes = selected.sizes().collect();
        let request = match selected.request() {
            Ok(request) => request,
            Err(refusal) => return Outcome::Refused(refusal.reason.code()),
        };
        // A slow local filesystem cannot replay a stale gesture after a close,
        // control loss or a later readiness transition. No retry is scheduled.
        if !self.ready_for(dropped) {
            return Outcome::Refused("file_selection_expired");
        }
        let Ok(source) = self.intake.submit(request) else {
            return Outcome::Refused("file_selection_unavailable");
        };
        // Every new native window holds this new immutable control. A hide or
        // failure racing after intake admission stops future drops, but cannot
        // cancel an already accepted transfer. Preserve its real later receipt.
        if !self.panel.replace_upload(source.clone()) {
            self.inbox.close();
        }
        Outcome::Submitted(source)
    }
    fn ready_for(&self, dropped: &Dropped) -> bool {
        dropped.captured.elapsed() < MAX_AGE
            && self.live()
            && self.intake.ready()
            && self.panel.status() == PanelStatus::Running
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shared.stop();
        self.inbox.close();
        self.shared.record_retired(&self.inbox);
        for batch in self.shared.batches().iter_mut() {
            if matches!(batch.outcome, Outcome::Preparing) {
                batch.outcome = Outcome::Refused("file_selection_stopped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, time::Instant};

    #[test]
    fn retired_intent_is_reported_once_after_stop_without_losing_earlier_results() {
        let shared = Arc::new(Shared::default());
        let control = Control(Arc::clone(&shared));
        shared.batches().push(Batch {
            sizes: vec![Some(5)],
            outcome: Outcome::Refused("send_unreadable"),
        });
        let inbox = Inbox::default();
        inbox.set_ready(true);
        assert!(inbox.publish(
            vec![
                PathBuf::from("/tmp/private-first"),
                PathBuf::from("/tmp/private-second")
            ],
            Instant::now(),
        ));
        // These are the same retirement/collection steps used by Worker::drop
        // even when its loop never observes the now-stopped accepted gesture.
        shared.stop();
        inbox.close();
        shared.record_retired(&inbox);
        shared.record_retired(&inbox);
        let batches = control.batches();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].sizes, [Some(5)]);
        assert_eq!(batches[0].status, Err("send_unreadable"));
        assert_eq!(batches[1].sizes, [None, None]);
        assert_eq!(batches[1].status, Err("file_selection_stopped"));
        assert!(!format!("{batches:?}").contains("private"));
        inbox.set_ready(true);
        assert!(!inbox.ready());
    }
}
