//! Repeated explicit local selections for the original desktop upload lane.
//!
//! Filesystem metadata/open calls run on one owned worker, never on the window
//! or authority thread. Its descriptors, result handles and bounded history
//! belong to this attempt only; reconnect must join this worker before opening
//! another desktop. The original sender remains the only source/transport owner.
#![forbid(unsafe_code)]

use crate::file_drop::{Dropped, Inbox, MAX_AGE};
#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
use crate::{
    file_progress::{Control as PanelControl, Status as PanelStatus},
    viewer_window::{Status as WindowStatus, WindowControl},
};
use frd::native_files::{Selection, SendControl, SendIntake, SendRequest, SendStatus};
use std::{
    fmt,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TURN: Duration = Duration::from_millis(10);
/// Bound content-free history as well as explicitly requested selections.
/// The host's cumulative transfer/byte limits still apply across every batch.
const MAX_BATCHES: usize = 8;

/// Native lifecycle and progress, without borrowing a window or calling a UI
/// toolkit from the selection worker. Both adapters bind one original surface.
pub(crate) trait Surface: Send + 'static {
    fn live(&self) -> bool;
    fn ready(&self) -> bool;
    /// Admit and retain the exact cancellation handle as one local operation.
    /// A native stop cannot fall between intake admission and handle retention.
    fn submit(
        &self,
        intake: &SendIntake,
        request: SendRequest,
        captured: Instant,
    ) -> Result<(SendControl, bool), &'static str>;
}

#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
struct X11Surface {
    window: WindowControl,
    panel: PanelControl,
}
#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
impl Surface for X11Surface {
    fn live(&self) -> bool {
        self.window.status() == WindowStatus::Mapped
            && !matches!(self.panel.status(), PanelStatus::Stopped(_))
    }
    fn ready(&self) -> bool {
        self.live() && self.panel.status() == PanelStatus::Running
    }
    fn submit(
        &self,
        intake: &SendIntake,
        request: SendRequest,
        captured: Instant,
    ) -> Result<(SendControl, bool), &'static str> {
        if captured.elapsed() >= MAX_AGE {
            return Err("file_selection_expired");
        }
        let source = intake
            .submit(request)
            .map_err(|_| "file_selection_unavailable")?;
        let live = self.panel.replace_upload(source.clone());
        Ok((source, live))
    }
}

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
    #[cfg(any(test, target_os = "macos"))]
    pub(crate) fn latest_selection(&self) -> Option<(u64, Result<SendControl, &'static str>)> {
        let batches = self.0.batches();
        batches.last().map(|batch| {
            (
                u64::try_from(batches.len()).expect("bounded original selections"),
                match &batch.outcome {
                    Outcome::Preparing => Err("file_selection_preparing"),
                    Outcome::Refused(code) => Err(*code),
                    Outcome::Submitted(control) => Ok(control.clone()),
                },
            )
        })
    }
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
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    pub(crate) fn start(
        window: WindowControl,
        intake: SendIntake,
        panel: PanelControl,
    ) -> Result<Self, Error> {
        let inbox = window.file_drop_inbox();
        Self::start_on(inbox, intake, X11Surface { window, panel })
    }
    pub(crate) fn start_on(
        inbox: Inbox,
        intake: SendIntake,
        surface: impl Surface,
    ) -> Result<Self, Error> {
        if !surface.live() || intake.is_closed() || inbox.is_closed() {
            return Err(Error::Unavailable);
        }
        let shared = Arc::new(Shared::default());
        let worker = Worker {
            shared: Arc::clone(&shared),
            inbox: inbox.clone(),
            intake,
            surface: Box::new(surface),
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
        if !self.is_finished() {
            return false;
        }
        let Some(thread) = self.thread.take() else {
            return true;
        };
        // Completion, including unwind, proves this sole selection worker no
        // longer retains paths/descriptors. It says nothing about remote effects.
        let _joined = thread.join();
        true
    }
    pub(crate) fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.stop();
        let _ = self.finish();
    }
}

struct Worker {
    shared: Arc<Shared>,
    inbox: Inbox,
    intake: SendIntake,
    surface: Box<dyn Surface>,
}
impl Worker {
    fn live(&self) -> bool {
        !self.shared.stopped()
            && !self.inbox.is_closed()
            && self.surface.live()
            && !self.intake.is_closed()
    }
    fn ready(&self) -> bool {
        self.live()
            && self.intake.ready()
            && self.surface.ready()
            && self.shared.batches().len() < MAX_BATCHES
    }
    fn run(self) {
        while self.live() {
            // Collect an already admitted intent before changing readiness.
            // Preparation rechecks the original lane; a stale/closed selection
            // gets its own result instead of being silently replaced.
            if let Some(dropped) = self.inbox.take() {
                self.prepare(&dropped);
            }
            self.shared.record_retired(&self.inbox);
            // Eligibility comes from the original lane and panel, NOT from
            // Inbox::ready (which is false while a valid drop waits in its slot).
            self.inbox.set_ready(self.ready());
            thread::sleep(TURN);
        }
    }
    fn prepare(&self, dropped: &Dropped) {
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
        let outcome = self.select(dropped, index);
        self.shared.batches()[index].outcome = outcome;
    }
    fn select(&self, dropped: &Dropped, index: usize) -> Outcome {
        let (sizes, request) = prepare_selection(dropped, || {
            self.live() && self.intake.ready() && self.surface.ready()
        });
        self.shared.batches()[index].sizes = sizes;
        let request = match request {
            Ok(request) => request,
            Err(code) => return Outcome::Refused(code),
        };
        let (source, live) = match self.surface.submit(&self.intake, request, dropped.captured) {
            Ok(admitted) => admitted,
            Err(code) => return Outcome::Refused(code),
        };
        // The surface has retained this actual control before a concurrent
        // local stop can finish. Keep its receipt even when the surface closes.
        if !live {
            self.inbox.close();
        }
        Outcome::Submitted(source)
    }
}

/// Production preparation boundary shared by both native desktops. These are
/// the only blocking path operations; the original gesture and admission gates
/// are checked on both sides, without extending either deadline or retrying.
fn prepare_selection(
    dropped: &Dropped,
    current: impl Fn() -> bool,
) -> (Vec<Option<u64>>, Result<SendRequest, &'static str>) {
    let ready = || dropped.captured.elapsed() < MAX_AGE && current();
    let unknown = || vec![None; dropped.paths.len()];
    if !ready() {
        return (unknown(), Err("file_selection_expired"));
    }
    let selected = match Selection::select(&dropped.paths) {
        Ok(selected) => selected,
        Err(refusal) => return (unknown(), Err(refusal.reason.code())),
    };
    let sizes = selected.sizes().collect();
    let request = match selected.request() {
        Ok(request) => request,
        Err(refusal) => return (sizes, Err(refusal.reason.code())),
    };
    if !ready() {
        return (sizes, Err("file_selection_expired"));
    }
    (sizes, Ok(request))
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
    fn production_preparation_pins_real_sources_and_checks_the_original_gate_twice() {
        use std::{cell::Cell, fs, sync::atomic::AtomicU64};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-selection-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let source = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        source.set_len(7).unwrap();
        let dropped = Dropped {
            paths: vec![path.clone()],
            captured: Instant::now(),
        };
        let checks = Cell::new(0);
        let (sizes, prepared) = prepare_selection(&dropped, || {
            checks.set(checks.get() + 1);
            checks.get() == 1 // original surface retires while descriptors are selected
        });
        assert_eq!(sizes, [Some(7)]);
        assert_eq!(prepared.unwrap_err(), "file_selection_expired");
        assert_eq!(checks.get(), 2);
        let (sizes, prepared) = prepare_selection(&dropped, || true);
        assert_eq!(sizes, [Some(7)]);
        assert!(prepared.is_ok());
        drop(prepared);
        fs::remove_file(&path).unwrap();
        // A closed/expired gesture refuses before touching a now-missing path.
        assert_eq!(
            prepare_selection(&dropped, || false).1.unwrap_err(),
            "file_selection_expired"
        );
        assert_eq!(
            prepare_selection(&dropped, || true).1.unwrap_err(),
            "send_missing"
        );
        let old = Dropped {
            paths: vec![path],
            captured: Instant::now().checked_sub(MAX_AGE).unwrap(),
        };
        assert_eq!(
            prepare_selection(&old, || true).1.unwrap_err(),
            "file_selection_expired"
        );
    }

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
