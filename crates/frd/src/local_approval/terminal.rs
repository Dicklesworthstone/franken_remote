//! Nonblocking /dev/tty I/O, confined to the single native-I/O worker. The
//! process-wide permit is released only after joining, not merely on a stop.
use super::{
    CANCELLED, CONSUMED, Error, Handle, Pending, Question, Shared, State, Status, WAITING,
};
use fr_core::input_submission::Capability;
use fr_wire::negotiation::Role;
use rustix::{
    fs::{Mode, OFlags, open},
    process::getpgrp,
    termios::{LocalModes, tcgetattr, tcgetpgrp},
};
use std::{
    fs::File,
    io::{self, IsTerminal, Read, Write},
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TURN: Duration = Duration::from_millis(5);
const MAX_PROMPT_BYTES: usize = 2048;
const MAX_LINE_BYTES: usize = 64;
static OCCUPIED: AtomicBool = AtomicBool::new(false);
// Unwinding/abandonment retains the actual handle. A later start must join it
// first; an uncooperative native call never creates unbounded replacement work.
static RETIRED: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

pub struct Owner {
    handle: Handle,
    worker: Option<JoinHandle<()>>,
}
impl Owner {
    pub fn start() -> Result<Self, Error> {
        reap_retired()?;
        OCCUPIED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State { status: Status::Opening, pending: None }),
        });
        let worker_shared = shared.clone();
        let worker = match thread::Builder::new().name("fr-local-approval".into()).spawn(move || {
            let _guard = Guard(worker_shared.clone());
            if let Err(error) = serve(&worker_shared) {
                worker_shared.stop(error);
            }
        }) {
            Ok(worker) => worker,
            Err(_) => {
                OCCUPIED.store(false, Ordering::Release);
                return Err(Error::Unavailable);
            }
        };
        Ok(Self { handle: Handle { shared }, worker: Some(worker) })
    }
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }
    pub fn status(&self) -> Status {
        self.handle.status()
    }
    /// Fence every queued/delivered decision synchronously, before joining.
    pub fn stop(&self) {
        self.handle.shared.stop(Error::Closed);
    }
    /// None means the ORIGINAL worker still owns native work. Never start a
    /// replacement on that evidence. A joined native refusal is clean cleanup.
    pub fn try_finish(&mut self) -> Option<Result<(), Error>> {
        if self.worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return None;
        }
        Some(self.worker.take().map_or(Ok(()), |worker| {
            let result = worker.join().map_err(|_| Error::Panicked);
            OCCUPIED.store(false, Ordering::Release);
            result
        }))
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let mut retired = RETIRED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // OCCUPIED permits exactly one live or retired owner.
            debug_assert!(retired.is_none());
            *retired = Some(worker);
        }
    }
}
fn reap_retired() -> Result<(), Error> {
    let mut retired = RETIRED.lock().map_err(|_| Error::Poisoned)?;
    if retired.as_ref().is_some_and(|worker| !worker.is_finished()) {
        return Err(Error::Busy);
    }
    if let Some(worker) = retired.take() {
        let result = worker.join().map_err(|_| Error::Panicked);
        OCCUPIED.store(false, Ordering::Release);
        result?;
    }
    Ok(())
}
struct Guard(Arc<Shared>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.stop(if thread::panicking() { Error::Panicked } else { Error::Closed });
    }
}
fn foreground(tty: &File) -> Result<(), Error> {
    if !tty.is_terminal()
        || tcgetpgrp(tty).map_err(|_| Error::Unavailable)? != getpgrp()
        || !tcgetattr(tty).map_err(|_| Error::Unavailable)?.local_modes.contains(LocalModes::ICANON)
    {
        return Err(Error::Unavailable);
    }
    Ok(())
}
fn serve(shared: &Shared) -> Result<(), Error> {
    // No autoselected path, stdin fallback, shell, terminal-setting change or
    // controlling-terminal acquisition. The descriptor never escapes this worker.
    let fd = open("/dev/tty", OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty()).map_err(|_| Error::Unavailable)?;
    let mut tty = File::from(fd);
    foreground(&tty)?;
    {
        let mut state = shared.state.lock().map_err(|_| Error::Poisoned)?;
        if state.status != Status::Opening {
            return Ok(());
        }
        state.status = Status::Ready;
    }
    let mut showing: Option<Presentation> = None;
    loop {
        let current = {
            let state = shared.state.lock().map_err(|_| Error::Poisoned)?;
            if state.status != Status::Ready {
                return Ok(());
            }
            state.pending.clone()
        };
        foreground(&tty)?;
        if let Some(pending) = current {
            match pending.state_at(Instant::now()) {
                CANCELLED | CONSUMED => {
                    showing = None;
                    let mut state = shared.state.lock().map_err(|_| Error::Poisoned)?;
                    if state.pending.as_ref().is_some_and(|old| Arc::ptr_eq(old, &pending)) {
                        state.pending = None;
                    }
                }
                WAITING => {
                    if showing.as_ref().is_none_or(|old| !Arc::ptr_eq(&old.pending, &pending)) {
                        showing = Some(Presentation::new(pending)?);
                    }
                    if let Some(showing) = &mut showing {
                        showing.turn(&mut tty, Instant::now)?;
                    }
                }
                // A delivered decision still occupies the slot until the exact
                // caller consumes it or drops its receipt. Do not read ahead.
                _ => {}
            }
        }
        // This is only native terminal progress, never an authority heartbeat.
        thread::sleep(TURN);
    }
}

pub(super) struct Presentation {
    pending: Arc<Pending>,
    output: String,
    written: usize,
    line: Line,
}
impl Presentation {
    pub(super) fn new(pending: Arc<Pending>) -> Result<Self, Error> {
        let mut output = description(pending.question);
        output.push_str(&format!(
            "\nType approve {:032x} then Enter to allow; any other line denies.\n> ",
            pending.challenge,
        ));
        if output.len() > MAX_PROMPT_BYTES {
            return Err(Error::Unavailable);
        }
        Ok(Self { pending, output, written: 0, line: Line::new() })
    }
    pub(super) fn turn(
        &mut self,
        tty: &mut (impl Read + Write),
        now: impl Fn() -> Instant,
    ) -> Result<(), Error> {
        if self.pending.state_at(now()) != WAITING {
            return Ok(());
        }
        if self.written < self.output.len() {
            match tty.write(&self.output.as_bytes()[self.written..]) {
                Ok(0) => return Err(Error::Unavailable),
                Ok(n) => self.written += n,
                Err(error) if retry(&error) => return Ok(()),
                Err(_) => return Err(Error::Unavailable),
            }
        }
        if self.written != self.output.len() || self.pending.state_at(now()) != WAITING {
            return Ok(());
        }
        let mut bytes = [0; 128];
        let count = match tty.read(&mut bytes) {
            // EOF is terminal loss, never a default yes.
            Ok(0) => return Err(Error::Unavailable),
            Ok(n) => n,
            Err(error) if retry(&error) => return Ok(()),
            Err(_) => return Err(Error::Unavailable),
        };
        for &byte in &bytes[..count] {
            if let Some(allow) = self.line.push(byte, self.pending.challenge) {
                // Account for time spent inside native read; caller additionally
                // rechecks original session/view evidence at receipt consumption.
                self.pending.decide_at(allow, now());
                break;
            }
        }
        Ok(())
    }
}
fn retry(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}
struct Line {
    bytes: [u8; MAX_LINE_BYTES],
    len: usize,
}
impl Line {
    fn new() -> Self {
        Self { bytes: [0; MAX_LINE_BYTES], len: 0 }
    }
    fn push(&mut self, byte: u8, challenge: u128) -> Option<bool> {
        if matches!(byte, b'\n' | b'\r') {
            let expected = format!("approve {challenge:032x}");
            return Some(&self.bytes[..self.len] == expected.as_bytes());
        }
        if self.len == self.bytes.len() {
            return Some(false);
        }
        self.bytes[self.len] = byte;
        self.len += 1;
        None
    }
}
fn description(question: Question) -> String {
    match question {
        Question::Observation { peer, binding, role } => format!(
            "\nFrankenRemote LOCAL APPROVAL\nAuthenticated tailnet endpoint: {peer}\nSession: {:032x}\nAllow this peer to VIEW this desktop?{}\nThis does NOT grant input control. Closing this host ends sharing.",
            binding.remote_session.as_raw(),
            if role == Role::RequestControl {
                " A separate prompt will ask for its exact input-control request."
            } else { "" },
        ),
        Question::Control(request) => {
            let target = request.target;
            let view = target.view;
            let operations = [
                (Capability::Keys, "keys"), (Capability::Repeat, "key-repeat"),
                (Capability::Absolute, "pointer"), (Capability::Buttons, "buttons"),
                (Capability::Relative, "relative-pointer"), (Capability::PixelScroll, "pixel-scroll"),
                (Capability::LineScroll, "line-scroll"), (Capability::Text, "text"),
            ].into_iter().filter_map(|(capability, name)| {
                target.capabilities.contains(capability).then_some(name)
            }).collect::<Vec<_>>().join(", ");
            format!(
                "\nFrankenRemote LOCAL INPUT APPROVAL\nPreviously admitted session: {:032x}\nRequest: {} / channel {} / display {}\nView generations: geometry={} viewport={} codec={} recovery={}\nDesktop: ({}, {}) {}x{}\nAllow remote operations: {operations}?\nOnly this exact request is covered; changed or lost view evidence cancels it.",
                request.parent.remote_session.as_raw(), request.sequence, request.parent.id,
                target.display_binding, view.geometry.as_raw(), view.viewport.as_raw(),
                view.configuration.as_raw(), view.recovery.as_raw(),
                target.bounds.origin().x, target.bounds.origin().y,
                target.bounds.width(), target.bounds.height(),
            )
        }
    }
}
