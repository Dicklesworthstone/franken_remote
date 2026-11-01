//! A local machine picker which exists before any remote session is created.
//!
//! Rows are immutable display data. A choice is only an ordinal into the
//! caller's retained discovery snapshot; it is neither peer authentication nor
//! permission to reuse an address. The caller must resolve that original peer
//! again before connecting. No network/session owner enters this adapter.

use std::{
    ffi::CString,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

mod native;
#[cfg(test)]
mod tests;

pub const MAX_ROWS: usize = 1024;
const MAP_LIMIT: Duration = Duration::from_secs(2);
const CHOICE_LIMIT: Duration = Duration::from_secs(120);

/// A discovery hint, not evidence about the next connection's actual route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathHint {
    Direct,
    Relayed,
    Unknown,
}

/// A bounded local row. Its contents are deliberately absent from `Debug`.
#[derive(Clone)]
pub struct Row {
    name: String,
    stable_id: String,
    path: PathHint,
}

impl Row {
    pub fn new(name: &str, stable_id: &str, path: PathHint) -> Result<Self, Error> {
        if name.is_empty()
            || name.len() > 254
            || !name.bytes().all(|byte| byte.is_ascii_graphic())
            || stable_id.is_empty()
            || stable_id.len() > 128
            || !stable_id.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(Error::InvalidRow);
        }
        Ok(Self {
            name: name.to_owned(),
            stable_id: stable_id.to_owned(),
            path,
        })
    }
}

impl fmt::Debug for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Row")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Content-free local failures. None of these imply that a peer was contacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Empty,
    Capacity,
    InvalidRow,
    DuplicateIdentity,
    InvalidDisplay,
    WorkerUnavailable,
    WorkerPanicked,
    NativeUnavailable,
    NativeChanged,
    EventFlood,
    MapTimeout,
    ChoiceTimeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "host picker: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Opening,
    Mapped,
    Closing,
    /// The original native worker has joined and its choice may be consumed.
    Chosen,
    Consumed,
    Cancelled,
    Failed(Error),
}

struct State {
    status: Status,
    mapped: bool,
    window: Option<u32>,
    choice: Option<usize>,
    joined: bool,
}

struct Shared {
    started: Instant,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            state.status,
            Status::Opening | Status::Mapped | Status::Closing | Status::Chosen
        ) {
            let elapsed = self.started.elapsed();
            let failure = if !state.mapped && elapsed >= MAP_LIMIT {
                Some(Error::MapTimeout)
            } else if elapsed >= CHOICE_LIMIT {
                Some(Error::ChoiceTimeout)
            } else {
                None
            };
            if let Some(error) = failure {
                state.status = Status::Failed(error);
                state.choice = None;
            }
        }
        state
    }

    fn live(&self) -> bool {
        matches!(self.state().status, Status::Opening | Status::Mapped)
    }

    fn finish(&self, result: Result<Option<usize>, Error>) {
        let mut state = self.state();
        state.window = None;
        if matches!(state.status, Status::Opening | Status::Mapped) {
            match result {
                Ok(Some(index)) => {
                    state.choice = Some(index);
                    state.status = Status::Closing;
                }
                Ok(None) => state.status = Status::Cancelled,
                Err(error) => state.status = Status::Failed(error),
            }
        }
    }

    fn joined(&self, panicked: bool) {
        let mut state = self.state();
        state.window = None;
        state.joined = true;
        if panicked && !matches!(state.status, Status::Cancelled | Status::Failed(_)) {
            state.status = Status::Failed(Error::WorkerPanicked);
            state.choice = None;
        } else if state.status == Status::Closing {
            state.status = Status::Chosen;
        }
    }
}

/// Cancels only this local window. Cancellation does not wait on X11 or a join.
#[derive(Clone)]
pub struct Control(Arc<Shared>);

impl Control {
    pub fn cancel(&self) {
        let mut state = self.0.state();
        if !matches!(state.status, Status::Consumed | Status::Failed(_)) {
            state.status = Status::Cancelled;
            state.choice = None;
        }
    }

    pub fn is_complete(&self) -> bool {
        self.0.state().joined
    }

    pub fn status(&self) -> Status {
        self.0.state().status
    }

    pub fn window(&self) -> Option<u32> {
        self.0.state().window
    }
}

impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostPickerControl")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// Owns one window and one immutable row order until cancellation or choice.
///
/// A fixed join-only supervisor owns the original native thread handle. Polls
/// never call `join` or mistake `is_finished` for completed thread-local
/// destruction. A stuck native call leaves cleanup incomplete and cannot
/// release a choice. The caller must also bound its wait for cleanup.
pub struct HostPicker {
    control: Control,
}

impl HostPicker {
    pub fn start(display: &str, rows: &[Row]) -> Result<Self, Error> {
        validate(display, rows)?;
        let display = CString::new(display).map_err(|_| Error::InvalidDisplay)?;
        let rows = native::rows(rows);
        let shared = Arc::new(Shared {
            started: Instant::now(),
            state: Mutex::new(State {
                status: Status::Opening,
                mapped: false,
                window: None,
                choice: None,
                joined: false,
            }),
        });
        let native_shared = Arc::clone(&shared);
        supervise(Arc::clone(&shared), move || {
            native::run(&native_shared, &display, &rows)
        })?;
        Ok(Self {
            control: Control(shared),
        })
    }

    pub fn control(&self) -> Control {
        self.control.clone()
    }

    /// Returns the chosen original ordinal exactly once, after actual join.
    pub fn poll(&mut self) -> Result<Option<usize>, Error> {
        let mut state = self.control.0.state();
        match state.status {
            Status::Failed(error) => Err(error),
            Status::Chosen if state.joined => {
                state.status = Status::Consumed;
                Ok(state.choice.take())
            }
            _ => Ok(None),
        }
    }

    pub fn cancel(&self) {
        self.control.cancel();
    }

    pub fn is_complete(&self) -> bool {
        self.control.is_complete()
    }

    pub fn status(&self) -> Status {
        self.control.status()
    }

    pub fn window(&self) -> Option<u32> {
        self.control.window()
    }
}

impl Drop for HostPicker {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn supervise(
    shared: Arc<Shared>,
    operation: impl FnOnce() -> Result<Option<usize>, Error> + Send + 'static,
) -> Result<(), Error> {
    thread::Builder::new()
        .name("fr-host-picker-join".into())
        .spawn(move || {
            let worker_shared = Arc::clone(&shared);
            if let Ok(worker) =
                thread::Builder::new()
                    .name("fr-host-picker".into())
                    .spawn(move || {
                        let result = operation();
                        worker_shared.finish(result);
                    })
            {
                shared.joined(worker.join().is_err());
            } else {
                shared.finish(Err(Error::WorkerUnavailable));
                shared.joined(false);
            }
        })
        .map(|_| ())
        .map_err(|_| Error::WorkerUnavailable)
}

fn validate(display: &str, rows: &[Row]) -> Result<(), Error> {
    let valid = |part: &str| {
        !part.is_empty() && part.len() <= 5 && part.bytes().all(|b| b.is_ascii_digit())
    };
    let mut parts = display
        .strip_prefix(':')
        .ok_or(Error::InvalidDisplay)?
        .split('.');
    if !parts.next().is_some_and(valid) || !parts.next().is_none_or(valid) || parts.next().is_some()
    {
        return Err(Error::InvalidDisplay);
    }
    if rows.is_empty() {
        return Err(Error::Empty);
    }
    if rows.len() > MAX_ROWS {
        return Err(Error::Capacity);
    }
    let mut names = std::collections::BTreeSet::new();
    let mut identities = std::collections::BTreeSet::new();
    for row in rows {
        if !names.insert(&row.name) || !identities.insert(&row.stable_id) {
            return Err(Error::DuplicateIdentity);
        }
    }
    Ok(())
}
