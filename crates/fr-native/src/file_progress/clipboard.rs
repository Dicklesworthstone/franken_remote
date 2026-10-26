//! The clipboard's original privacy surface and its independently supervised
//! mapping/progress evidence. Neither waiting nor XCB runs on the authority path.
use super::{Control, Error, Panel, Status, StopReason, TURN};
use fr_core::clipboard::PlatformError;
use frd::{
    native_clipboard::{Configuration, Control as ClipboardControl},
    session_startup::StreamingViewerControl,
};
use std::{
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

const MAP_TIMEOUT: Duration = Duration::from_secs(2);
const PROGRESS_TIMEOUT: Duration = Duration::from_millis(250);

pub(super) struct Privacy {
    source: ClipboardControl,
    started: Instant,
    last_turn: Mutex<Instant>,
}
impl Privacy {
    pub(super) fn new(source: ClipboardControl) -> Self {
        let started = Instant::now();
        Self {
            source,
            started,
            last_turn: Mutex::new(started),
        }
    }
    pub(super) fn stop(&self) {
        // This fences final publication and queued egress synchronously. It
        // cannot revoke input, close video or reach a replacement session.
        self.source.stop();
    }
    pub(super) fn progress(&self) {
        match self.last_turn.lock() {
            Ok(mut last) => *last = Instant::now(),
            Err(_) => self.stop(),
        }
    }
    fn check(&self, status: Status, now: Instant) -> Result<(), StopReason> {
        let last = *self
            .last_turn
            .lock()
            .map_err(|_| StopReason::NativeFailure)?;
        progress_check(status, now, self.started, last)
    }
}

fn progress_check(
    status: Status,
    now: Instant,
    started: Instant,
    last: Instant,
) -> Result<(), StopReason> {
    match status {
        Status::Opening if now.saturating_duration_since(started) >= MAP_TIMEOUT => {
            Err(StopReason::MappingExpired)
        }
        Status::Running if now.saturating_duration_since(last) >= PROGRESS_TIMEOUT => {
            Err(StopReason::Unresponsive)
        }
        Status::Stopped(reason) => Err(reason),
        Status::Opening | Status::Running => Ok(()),
    }
}

pub(super) fn supervise(control: &Control) {
    let Some(privacy) = &control.privacy else {
        return;
    };
    loop {
        if let Err(reason) = privacy.check(control.status(), Instant::now()) {
            control.stop(reason);
            return;
        }
        thread::sleep(TURN);
    }
}

fn wait_ready(control: &Control) -> Result<(), Error> {
    let Some(privacy) = &control.privacy else {
        return Err(Error::Stopped(StopReason::NativeFailure));
    };
    loop {
        let status = control.status();
        if let Err(reason) = privacy.check(status, Instant::now()) {
            control.stop(reason);
            return Err(Error::Stopped(reason));
        }
        if !privacy.source.status().enabled {
            control.stop(StopReason::OwnerStopped);
            return Err(Error::Stopped(StopReason::OwnerStopped));
        }
        if status == Status::Running {
            return Ok(());
        }
        thread::sleep(TURN);
    }
}

#[derive(Default)]
struct Setup {
    closed: bool,
    starting: bool,
    panel: Option<Result<Panel, Error>>,
}
fn lock(shared: &Mutex<Setup>) -> MutexGuard<'_, Setup> {
    shared.lock().unwrap_or_else(|poison| {
        let mut state = poison.into_inner();
        state.closed = true;
        if let Some(Ok(panel)) = &state.panel {
            panel.stop();
        }
        state
    })
}

/// Retained by the ORIGINAL Desktop before any service can launch its factory.
/// The worker closure borrows shared ownership, but cannot discard the UI task
/// after setup failure. Reconnection waits for both original native threads.
pub(crate) struct Owner(Arc<Mutex<Setup>>);
impl Owner {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(Setup::default())))
    }
    pub(crate) fn protect(
        &self,
        configuration: Configuration,
        display: String,
        parent: Result<crate::viewer_input::Window, Error>,
        session: StreamingViewerControl,
    ) -> Configuration {
        let shared = self.0.clone();
        configuration.before_native_setup(move |source| {
            {
                let mut state = lock(&shared);
                if state.closed || state.starting || state.panel.is_some() {
                    source.stop();
                    return Err(PlatformError::Unavailable);
                }
                state.starting = true;
            }
            // The hook runs on the existing clipboard worker, within that
            // worker's original setup deadline, before the native factory.
            let panel = parent.and_then(|parent| {
                Panel::start_clipboard(&display, parent, session, source.clone())
            });
            let control = panel.as_ref().map(Panel::control).map_err(|error| *error);
            {
                let mut state = lock(&shared);
                if state.closed
                    && let Ok(panel) = &panel
                {
                    panel.stop();
                }
                state.panel = Some(panel);
                state.starting = false;
            }
            if control.and_then(|control| wait_ready(&control)).is_err() {
                source.stop();
                return Err(PlatformError::Unavailable);
            }
            Ok(())
        })
    }
    pub(crate) fn control(&self) -> Option<Result<Control, Error>> {
        lock(&self.0)
            .panel
            .as_ref()
            .map(|panel| panel.as_ref().map(Panel::control).map_err(|error| *error))
    }
    pub(crate) fn stop(&self) {
        let mut state = lock(&self.0);
        state.closed = true;
        if let Some(Ok(panel)) = &state.panel {
            panel.stop();
        }
    }
    pub(crate) fn finish(&mut self) -> bool {
        let mut state = lock(&self.0);
        !state.starting
            && state.panel.as_mut().is_none_or(|panel| match panel {
                Ok(panel) => panel.finish(),
                Err(_) => true,
            })
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicU8, mpsc};

    #[test]
    fn opening_progress_cannot_extend_mapping_deadline_and_stale_ui_cannot_authorize_access() {
        let start = Instant::now();
        let mapped = start + MAP_TIMEOUT - Duration::from_millis(1);
        assert_eq!(
            progress_check(Status::Opening, mapped, start, mapped),
            Ok(())
        );
        assert_eq!(
            progress_check(
                Status::Opening,
                start + MAP_TIMEOUT,
                start,
                start + MAP_TIMEOUT
            ),
            Err(StopReason::MappingExpired)
        );
        assert_eq!(
            progress_check(Status::Running, mapped, start, mapped),
            Ok(())
        );
        assert_eq!(
            progress_check(Status::Running, mapped + PROGRESS_TIMEOUT, start, mapped),
            Err(StopReason::Unresponsive)
        );
        assert_eq!(
            progress_check(Status::Stopped(StopReason::Hidden), mapped, start, mapped),
            Err(StopReason::Hidden)
        );
    }

    #[test]
    fn owner_retains_and_reaps_both_threads_after_a_stop() {
        let mut owner = Owner::new();
        let control = Control {
            state: Arc::new(AtomicU8::new(0)),
            privacy: None,
        };
        let (native_release, native_wait) = mpsc::sync_channel::<()>(1);
        let (watch_release, watch_wait) = mpsc::sync_channel::<()>(1);
        let native = thread::spawn(move || native_wait.recv().unwrap());
        let watchdog = thread::spawn(move || watch_wait.recv().unwrap());
        lock(&owner.0).panel = Some(Ok(Panel {
            control: control.clone(),
            task: Some(native),
            watchdog: Some(watchdog),
        }));
        owner.stop();
        assert_eq!(control.status(), Status::Stopped(StopReason::OwnerStopped));
        assert!(!owner.finish());
        native_release.send(()).unwrap();
        assert!(!owner.finish());
        watch_release.send(()).unwrap();
        super::super::tests::wait(|| owner.finish());
        assert!(owner.finish());
        // A late UI handle stays terminal and cannot restart an owner.
        control.hide();
        assert_eq!(control.status(), Status::Stopped(StopReason::OwnerStopped));
    }
}
