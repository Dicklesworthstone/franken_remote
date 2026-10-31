//! Bounded automation on the original credential-checked local owner socket.
use super::{Cx, Deadline, Error, UnixStream, bounded, identified};
use crate::robot_input::{Request, Response};
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use std::time::{Duration, Instant};
#[cfg(any(feature = "linux-desktop", test))]
use {
    crate::robot_input::{Refusal, Snapshot},
    fr_client::robot::input::native::{self, Admission, Ledger, RequestKey, Scope},
    fr_core::input::InputView,
    std::future::poll_fn,
    std::sync::{Arc, Mutex, MutexGuard},
    std::task::{Poll, Waker},
};

pub(super) const STATUS: [u8; 8] = *b"FRSC1IS\0";
pub(super) const SUBMIT: [u8; 8] = *b"FRSC1IA\0";
const REPLY: [u8; 8] = *b"FRSC1IR\0";
const MAX_REQUEST: usize = 512;
const MAX_REPLY: usize = 2048;

/// The client authenticates and compares the exact invocation before sending
/// anything. No retry, alternate owner, remote dial or fresh input ticket here.
pub(crate) async fn exchange(
    cx: &Cx,
    target: &str,
    expected: &str,
    request: Option<Request>,
) -> Result<Response, Error> {
    crate::robot_input::id(expected).map_err(|_| Error::InvalidInstance)?;
    let began = Instant::now();
    let (mut stream, info, instance) = identified(cx, target).await?;
    if info.instance != expected {
        return Err(Error::WrongOwner);
    }
    let body = request
        .map(|r| serde_json::to_vec(&r))
        .transpose()
        .map_err(|_| Error::Protocol)?
        .unwrap_or_default();
    if body.len() > MAX_REQUEST || request.is_some_and(|r| !r.valid()) {
        return Err(Error::Protocol);
    }
    let mut header = [0; 24];
    header[..8].copy_from_slice(if request.is_some() { &SUBMIT } else { &STATUS });
    header[8..].copy_from_slice(&instance);
    let deadline = Deadline::new(cx, Duration::from_secs(3))?;
    // After this point a failed request may have reached the original owner.
    let result = bounded(cx, deadline, async {
        stream
            .write_all(&header)
            .await
            .map_err(|_| Error::Unavailable)?;
        if request.is_some() {
            stream
                .write_all(
                    &u16::try_from(body.len())
                        .map_err(|_| Error::Protocol)?
                        .to_be_bytes(),
                )
                .await
                .map_err(|_| Error::Unavailable)?;
            stream
                .write_all(&body)
                .await
                .map_err(|_| Error::Unavailable)?;
        }
        let mut header = [0; 26];
        stream
            .read_exact(&mut header)
            .await
            .map_err(|_| Error::Unavailable)?;
        if header[..8] != REPLY || header[8..24] != instance {
            return Err(Error::Protocol);
        }
        let len = usize::from(u16::from_be_bytes([header[24], header[25]]));
        if len == 0 || len > MAX_REPLY {
            return Err(Error::Protocol);
        }
        let mut bytes = [0; MAX_REPLY];
        stream
            .read_exact(&mut bytes[..len])
            .await
            .map_err(|_| Error::Unavailable)?;
        let mut reply: Response =
            serde_json::from_slice(&bytes[..len]).map_err(|_| Error::Protocol)?;
        match (&mut reply, request) {
            (Response::Result { report }, Some(request)) => {
                validate_report(*report, request)?;
            }
            (Response::Status { snapshot }, None) => {
                validate_snapshot(snapshot)?;
                if let Some(age) = snapshot.source_age_upper_ms.as_mut() {
                    *age = age
                        .checked_add(
                            u64::try_from(began.elapsed().as_nanos().div_ceil(1_000_000))
                                .map_err(|_| Error::Protocol)?,
                        )
                        .ok_or(Error::Protocol)?;
                    // The owner sampled before the round trip. Never expose an
                    // expired observation as currently ready at the reader.
                    if *age >= 1000 {
                        clear_snapshot(snapshot);
                    }
                }
            }
            (Response::Refused { .. }, _) => {}
            _ => return Err(Error::Protocol),
        }
        Ok(reply)
    })
    .await;
    result.map_err(|error| {
        if request.is_some() {
            Error::OutcomeUnknown
        } else {
            error
        }
    })
}
pub(super) fn validate_snapshot(snapshot: &crate::robot_input::Snapshot) -> Result<(), Error> {
    let complete = snapshot
        .lease
        .as_deref()
        .is_some_and(|lease| crate::robot_input::id(lease).is_ok())
        && [snapshot.observation_id, snapshot.next_request_id]
            .into_iter().all(|v| v.is_some_and(|n| n != 0))
        // The protocol starts generation counters at zero; frame zero is also
        // a valid presented stamp. Presence, not an invented nonzero rule.
        && [snapshot.geometry_generation, snapshot.viewport_generation,
            snapshot.configuration_generation, snapshot.recovery_generation,
            snapshot.frame].into_iter().all(|v| v.is_some())
        && snapshot.source_age_upper_ms.is_some_and(|age| age < 1000);
    let empty = snapshot.lease.is_none()
        && [
            snapshot.observation_id,
            snapshot.geometry_generation,
            snapshot.viewport_generation,
            snapshot.configuration_generation,
            snapshot.recovery_generation,
            snapshot.frame,
            snapshot.source_age_upper_ms,
        ]
        .into_iter()
        .all(|v| v.is_none());
    if snapshot.next_request_id == Some(0)
        || snapshot.physical_visibility_proven
        || !(if snapshot.ready { complete } else { empty })
    {
        return Err(Error::Protocol);
    }
    Ok(())
}
fn validate_report(
    report: fr_client::robot::input::native::Report,
    request: Request,
) -> Result<(), Error> {
    if report.request_id != request.request_id || report.actions_total != request.operation.count()
    {
        return Err(Error::Protocol);
    }
    report.validate().map_err(|_| Error::Protocol)
}
pub(super) fn clear_snapshot(snapshot: &mut crate::robot_input::Snapshot) {
    snapshot.ready = false;
    snapshot.lease = None;
    snapshot.observation_id = None;
    snapshot.geometry_generation = None;
    snapshot.viewport_generation = None;
    snapshot.configuration_generation = None;
    snapshot.recovery_generation = None;
    snapshot.frame = None;
    snapshot.source_age_upper_ms = None;
}

#[cfg(any(feature = "linux-desktop", test))]
#[derive(Clone, Copy)]
struct Candidate {
    view: InputView,
    sample: fr_wire::presented::Sample,
    at: Instant,
}
#[cfg(any(feature = "linux-desktop", test))]
impl Candidate {
    fn age(self, at: Instant) -> Option<u64> {
        self.sample.age_upper_us.checked_add(
            u64::try_from(
                at.checked_duration_since(self.at)?
                    .as_nanos()
                    .div_ceil(1000),
            )
            .ok()?,
        )
    }
}
#[cfg(any(feature = "linux-desktop", test))]
struct Pending {
    request: Request,
    key: RequestKey,
    original: Candidate,
    encoded: u8,
}
#[cfg(any(feature = "linux-desktop", test))]
struct State {
    created: Instant,
    scope: Scope,
    lease: [u8; 16],
    ledger: Ledger,
    live: Option<Candidate>,
    pinned: Option<(u64, Candidate)>,
    next_observation: u64,
    observation: fr_client::robot::live::Observation,
    snapshots: Option<fr_client::robot::snapshot::Handle>,
    pending: Option<Pending>,
    stop: Option<frd::session_startup::ViewerControl>,
    stopped: bool,
    waiters: [Option<Waker>; 4],
}
#[cfg(any(feature = "linux-desktop", test))]
impl State {
    fn now(&self, at: Instant) -> Result<u64, Refusal> {
        u64::try_from(
            at.checked_duration_since(self.created)
                .ok_or(Refusal::Stopped)?
                .as_micros(),
        )
        .map_err(|_| Refusal::Stopped)
    }
    fn wake(&mut self) {
        for waker in &mut self.waiters {
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
    }
    fn retire(&mut self) {
        self.stopped = true;
        self.live = None;
        self.pinned = None;
        self.observation.retire(fr_client::robot::live::Phase::Stopped);
        if let Some(stop) = &self.stop {
            stop.stop();
        }
        self.ledger.retire();
        self.pending = None;
        self.wake();
    }
    fn empty(&self) -> Snapshot {
        Snapshot {
            ready: false,
            lease: None,
            observation_id: None,
            next_request_id: self.ledger.consumed_through().checked_add(1),
            geometry_generation: None,
            viewport_generation: None,
            configuration_generation: None,
            recovery_generation: None,
            frame: None,
            source_age_upper_ms: None,
            physical_visibility_proven: false,
        }
    }
    fn status(&mut self) -> Snapshot {
        if self
            .stop
            .as_ref()
            .is_some_and(frd::session_startup::ViewerControl::is_stopped)
        {
            self.retire();
        }
        let Some(candidate) = self
            .live
            .filter(|_| !self.stopped && !self.ledger.is_closed())
        else {
            return self.empty();
        };
        let Some(age) = candidate
            .age(Instant::now())
            .map(|age| age.div_ceil(1000))
            .filter(|age| *age < 1000)
        else {
            return self.empty();
        };
        if self.ledger.consumed_through().checked_add(1).is_none() {
            self.retire();
            return self.empty();
        }
        let Some(next) = self.next_observation.checked_add(1) else {
            self.retire();
            return self.empty();
        };
        self.next_observation = next;
        self.pinned = Some((next, candidate));
        Snapshot {
            ready: true,
            lease: Some(super::hex(&self.lease)),
            observation_id: Some(next),
            next_request_id: self.ledger.consumed_through().checked_add(1),
            geometry_generation: Some(candidate.view.geometry.as_raw()),
            viewport_generation: Some(candidate.view.viewport.as_raw()),
            configuration_generation: Some(candidate.view.configuration.as_raw()),
            recovery_generation: Some(candidate.view.recovery.as_raw()),
            frame: Some(candidate.sample.stamp.frame),
            source_age_upper_ms: Some(age),
            physical_visibility_proven: false,
        }
    }
    fn key(request: Request) -> Result<RequestKey, Refusal> {
        let bytes = serde_json::to_vec(&request).map_err(|_| Refusal::InvalidRequest)?;
        if bytes.len() > MAX_REQUEST {
            return Err(Refusal::InvalidRequest);
        }
        Ok(RequestKey {
            id: request.request_id,
            fingerprint: fr_client::robot::sha256::sha256_digest(&bytes),
        })
    }
    fn lookup(&self, key: RequestKey) -> Result<Option<Response>, Refusal> {
        match self.ledger.lookup(self.scope, key).map_err(ledger_error)? {
            Some(Admission::Retained(report)) => Ok(Some(Response::Result { report })),
            Some(Admission::Pending) | None => Ok(None),
            Some(Admission::Started) => Err(Refusal::InvalidRequest),
        }
    }
    fn submit(&mut self, request: Request) -> Result<Option<Response>, Refusal> {
        if self
            .stop
            .as_ref()
            .is_some_and(frd::session_startup::ViewerControl::is_stopped)
        {
            self.retire();
        }
        if !request.valid() {
            return Err(Refusal::InvalidRequest);
        }
        if request.lease != self.lease {
            return Err(Refusal::LeaseMismatch);
        }
        let key = Self::key(request)?;
        if let Some(reply) = self.lookup(key)? {
            return Ok(Some(reply));
        }
        if self.pending.as_ref().is_some_and(|p| p.key == key) {
            return Ok(None);
        }
        if self.stopped || self.ledger.is_closed() {
            return Err(Refusal::Stopped);
        }
        let now = self.now(Instant::now())?;
        let admission = self.ledger.begin(
            self.scope,
            key,
            now,
            now.checked_add(2_000_000).ok_or(Refusal::Stopped)?,
            request.operation.count(),
        );
        self.finish_if_retained();
        match admission.map_err(ledger_error)? {
            Admission::Retained(report) => return Ok(Some(Response::Result { report })),
            Admission::Pending => return Ok(None),
            Admission::Started => {}
        }
        let checked = (|| {
            let (id, original) = self.pinned.ok_or(Refusal::ObservationUnknown)?;
            if request.observation_id != id {
                return Err(Refusal::ObservationUnknown);
            }
            if original
                .age(Instant::now())
                .is_none_or(|age| age >= u64::from(request.max_observation_age_ms) * 1000)
            {
                return Err(Refusal::ObservationExpired);
            }
            let current = self.live.ok_or(Refusal::InputUnavailable)?;
            if current.view != original.view {
                return Err(Refusal::ViewChanged);
            }
            Ok(original)
        })();
        match checked {
            Ok(original) => {
                self.pending = Some(Pending {
                    request,
                    key,
                    original,
                    encoded: 0,
                });
                Ok(None)
            }
            Err(reason) => {
                let _ = self.ledger.reject(key.id);
                self.wake();
                Err(reason)
            }
        }
    }
    fn finish_if_retained(&mut self) {
        if self.pending.as_ref().is_some_and(|pending| {
            matches!(
                self.ledger.lookup(self.scope, pending.key),
                Ok(Some(Admission::Retained(_)))
            )
        }) {
            self.pending = None;
            self.wake();
        }
        if self.ledger.is_closed() {
            self.retire();
        }
    }
}
#[cfg(any(feature = "linux-desktop", test))]
fn ledger_error(error: native::Error) -> Refusal {
    match error {
        native::Error::RequestChanged => Refusal::RequestChanged,
        native::Error::Consumed => Refusal::Consumed,
        native::Error::ActionPending => Refusal::Busy,
        native::Error::Closed | native::Error::ClockRegression => Refusal::Stopped,
        _ => Refusal::InvalidRequest,
    }
}
#[cfg(any(feature = "linux-desktop", test))]
#[derive(Clone)]
pub(crate) struct Shared(Arc<Mutex<State>>);
#[cfg(any(feature = "linux-desktop", test))]
impl Shared {
    pub fn new(connection: [u8; 16], lease: [u8; 16]) -> Result<Self, Error> {
        let scope = Scope {
            connection,
            attempt: 1,
            lease_generation: 1,
        };
        if lease == [0; 16] {
            return Err(Error::Unavailable);
        }
        let ledger = Ledger::new(scope).map_err(|_| Error::Unavailable)?;
        Ok(Self(Arc::new(Mutex::new(State {
            created: Instant::now(),
            scope,
            lease,
            ledger,
            live: None,
            pinned: None,
            next_observation: 0,
            observation: fr_client::robot::live::Observation::default(),
            snapshots: None,
            pending: None,
            stop: None,
            stopped: false,
            waiters: std::array::from_fn(|_| None),
        }))))
    }
    // The owner, local peers and viewer callbacks run on one current-thread
    // runtime. Every borrow is synchronous and no guard survives an await or
    // invokes another Shared entry point. An authentic receipt or terminal
    // cleanup must never disappear because of a failed best-effort lock.
    fn locked(&self) -> MutexGuard<'_, State> {
        match self.0.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.retire();
                state
            }
        }
    }
    pub fn retire(&self) {
        self.locked().retire();
    }
    pub fn result(&self, event: fr_client::input::ResultEvent) {
        let mut state = self.locked();
        if state.ledger.result(event).is_err() {
            state.retire();
        }
        state.finish_if_retained();
        state.wake();
    }
    fn status(&self) -> Response {
        Response::Status {
            snapshot: self.locked().status(),
        }
    }
    async fn submit(&self, request: Request) -> Response {
        let key = match State::key(request) {
            Ok(key) => key,
            Err(reason) => return Response::Refused { reason },
        };
        let initial = self.locked().submit(request);
        match initial {
            Ok(Some(reply)) => return reply,
            Err(reason) => return Response::Refused { reason },
            Ok(None) => {}
        }
        let mut waiting = Waiting {
            shared: self.clone(),
            key,
            armed: true,
        };
        let reply = poll_fn(|task| {
            let mut state = self.locked();
            match state.lookup(key) {
                Ok(Some(reply)) => return Poll::Ready(reply),
                Err(reason) => return Poll::Ready(Response::Refused { reason }),
                Ok(None) if state.stopped => {
                    return Poll::Ready(Response::Refused {
                        reason: Refusal::Stopped,
                    });
                }
                Ok(None) => {}
            }
            if !state
                .waiters
                .iter()
                .flatten()
                .any(|w| w.will_wake(task.waker()))
                && let Some(slot) = state.waiters.iter_mut().find(|w| w.is_none())
            {
                *slot = Some(task.waker().clone());
            }
            Poll::Pending
        })
        .await;
        waiting.armed = false;
        reply
    }
}
/// Dropping a timed-out local peer cannot leave an unfinished request live on
/// the original owner. Completed receipts remain available for exact-ID reads.
#[cfg(any(feature = "linux-desktop", test))]
struct Waiting {
    shared: Shared,
    key: RequestKey,
    armed: bool,
}
#[cfg(any(feature = "linux-desktop", test))]
impl Drop for Waiting {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self.shared.locked();
            if state.pending.as_ref().is_some_and(|p| p.key == self.key) {
                state.retire();
            }
        }
    }
}
#[cfg(any(feature = "linux-desktop", test))]
pub(super) async fn respond(
    stream: &mut UnixStream,
    instance: [u8; 16],
    submit: bool,
    shared: Option<&Shared>,
) -> Result<(), Error> {
    let reply = if submit {
        let mut size = [0; 2];
        stream
            .read_exact(&mut size)
            .await
            .map_err(|_| Error::Protocol)?;
        let length = usize::from(u16::from_be_bytes(size));
        if length == 0 || length > MAX_REQUEST {
            return Err(Error::Protocol);
        }
        let mut bytes = [0; MAX_REQUEST];
        stream
            .read_exact(&mut bytes[..length])
            .await
            .map_err(|_| Error::Protocol)?;
        let request = serde_json::from_slice(&bytes[..length]).map_err(|_| Error::Protocol)?;
        match shared {
            Some(shared) => shared.submit(request).await,
            None => Response::Refused {
                reason: Refusal::Unavailable,
            },
        }
    } else {
        shared.map_or(
            Response::Refused {
                reason: Refusal::Unavailable,
            },
            Shared::status,
        )
    };
    let bytes = serde_json::to_vec(&reply).map_err(|_| Error::Protocol)?;
    if bytes.len() > MAX_REPLY {
        return Err(Error::Protocol);
    }
    let mut header = [0; 26];
    header[..8].copy_from_slice(&REPLY);
    header[8..24].copy_from_slice(&instance);
    header[24..].copy_from_slice(
        &u16::try_from(bytes.len())
            .map_err(|_| Error::Protocol)?
            .to_be_bytes(),
    );
    stream
        .write_all(&header)
        .await
        .map_err(|_| Error::Unavailable)?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|_| Error::Unavailable)
}

#[cfg(feature = "linux-desktop")]
#[path = "input/live.rs"]
mod live;

#[cfg(test)]
#[path = "input/tests.rs"]
mod tests;

#[cfg(any(feature = "linux-desktop", test))]
#[path = "input/screenshot.rs"]
mod screenshot;
