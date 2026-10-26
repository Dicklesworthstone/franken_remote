//! Clipboard-only native privacy lifetime. The UI and watchdog remain
//! independent of the thread doing X11 clipboard work. Closing the private
//! signal socket fences the original frd owner without waiting for a reply.
use fr_native::sharing_indicator::{self, IndicatorControl, LocalIndicator, Status};
use std::{
    io::{self, Read},
    net::Shutdown,
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TURN: Duration = Duration::from_millis(10);
const MAP_WAIT: Duration = Duration::from_millis(2_500);

struct State {
    stopped: AtomicBool,
    signal: UnixStream,
}
impl State {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        // No UI/native lock or allocation. EOF on this separate socket wakes
        // frd's stop monitor even while the command socket is blocked.
        let _ = self.signal.shutdown(Shutdown::Both);
    }
}
struct Fence(Arc<State>);
impl Drop for Fence {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// Same original native privacy state, retained by the clipboard's final
/// read/publication gate. It owns no thread and cannot re-enable the surface.
pub(super) struct Gate {
    state: Arc<State>,
    control: IndicatorControl,
}
impl Gate {
    pub(super) fn live(&self) -> bool {
        live(&self.state, &self.control)
    }
}
fn live(state: &State, control: &IndicatorControl) -> bool {
    if state.stopped.load(Ordering::Acquire) || !control.responsive() {
        state.stop();
        return false;
    }
    true
}

pub(super) struct Privacy {
    state: Arc<State>,
    indicator: LocalIndicator,
    control: IndicatorControl,
    task: Option<JoinHandle<()>>,
}
impl Privacy {
    pub(super) fn start(display: &str) -> Result<Self, ()> {
        let signal = UnixStream::from(io::stdout().as_fd().try_clone_to_owned().map_err(drop)?);
        // The role accepts only the parent's connected private socket, never
        // a file, tty, public listener or an inherited process-global label.
        signal.peer_addr().map_err(drop)?;
        signal.set_read_timeout(Some(TURN)).map_err(drop)?;
        let state = Arc::new(State {
            stopped: AtomicBool::new(false),
            signal,
        });
        let revoke = state.clone();
        let indicator = sharing_indicator::start_clipboard_with(display, move |_| revoke.stop())
            .map_err(drop)?;
        let control = indicator.control();
        let mut owner = Self {
            state,
            indicator,
            control,
            task: None,
        };
        let started = Instant::now();
        loop {
            match owner.control.status() {
                Status::Mapped if owner.live() => break,
                Status::Opening if started.elapsed() < MAP_WAIT => thread::sleep(TURN),
                _ => return Err(()),
            }
        }
        let state = owner.state.clone();
        let control = owner.control.clone();
        owner.task = Some(
            thread::Builder::new()
                .name("fr-clipboard-ui-watch".into())
                .spawn(move || {
                    let _fence = Fence(state.clone());
                    let mut signal = &state.signal;
                    let mut fixed = [0_u8; 1];
                    loop {
                        if state.stopped.load(Ordering::Acquire) || !control.responsive() {
                            return;
                        }
                        match signal.read(&mut fixed) {
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::WouldBlock
                                        | io::ErrorKind::TimedOut
                                        | io::ErrorKind::Interrupted
                                ) => {}
                            // Parent loss, any byte or any other channel fault
                            // is a terminal clipboard-only fence.
                            _ => return,
                        }
                    }
                })
                .map_err(drop)?,
        );
        Ok(owner)
    }
    pub(super) fn live(&self) -> bool {
        live(&self.state, &self.control)
    }
    pub(super) fn gate(&self) -> Gate {
        Gate {
            state: self.state.clone(),
            control: self.control.clone(),
        }
    }
}
impl Drop for Privacy {
    fn drop(&mut self) {
        self.state.stop();
        self.control.stop();
        if let Some(task) = self.task.take() {
            // stop() shut down the original socket, waking its only read.
            let _ = task.join();
        }
        // Native UI cleanup is collected when ready. A hung XCB call remains
        // confined to this child; frd retains and kills/reaps the whole child
        // before reporting its original clipboard worker cleanup complete.
        let _ = self.indicator.finish();
    }
}
