//! Capacity-one local terminal consent. No network listener, subprocess shell,
//! native input, capture or authority lives here. The owning host must recheck
//! its ORIGINAL approval/view capability before consuming a decision.
//!
//! All terminal I/O runs on one bounded, explicitly joined native-I/O worker.
//! It opens only the controlling /dev/tty, nonblocking, without changing terminal
//! settings. It never reads stdin, accepts a peer-selected path or treats EOF as
//! approval. A fresh challenge prevents queued text from approving another prompt.
mod terminal;
#[cfg(test)]
mod tests;

use fr_wire::{control::Request, negotiation::{ControlBinding, Role}};
use std::{
    fmt,
    net::SocketAddr,
    sync::{Arc, Mutex, atomic::{AtomicU8, Ordering}},
    time::{Duration, Instant},
};
pub use terminal::Owner;

const WAITING: u8 = 0;
const ALLOWED: u8 = 1;
const DENIED: u8 = 2;
const CANCELLED: u8 = 3;
const CONSUMED: u8 = 4;
/// A local UI bound, never an extension of the caller's shorter deadline.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Busy,
    Unavailable,
    Closed,
    Expired,
    Entropy,
    Poisoned,
    Panicked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Ready,
    Stopped(Error),
}

/// Immutable context made by the local host, not display text sent by a peer.
/// The second question identifies ALL fields of the exact pending input request.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Question {
    Observation {
        peer: SocketAddr,
        binding: ControlBinding,
        role: Role,
    },
    Control(Request),
}
impl fmt::Debug for Question {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalApprovalQuestion([local context])")
    }
}
struct Pending {
    question: Question,
    challenge: u128,
    until: Instant,
    state: AtomicU8,
}
impl Pending {
    fn state_at(&self, now: Instant) -> u8 {
        if now >= self.until {
            // Even a delivered but unconsumed yes expires at the ORIGINAL time.
            let _ = self.state.fetch_update(Ordering::AcqRel, Ordering::Acquire, |s| {
                matches!(s, WAITING | ALLOWED | DENIED).then_some(CANCELLED)
            });
        }
        self.state.load(Ordering::Acquire)
    }
    fn decide_at(&self, allow: bool, now: Instant) {
        if self.state_at(now) == WAITING {
            let _ = self.state.compare_exchange(
                WAITING,
                if allow { ALLOWED } else { DENIED },
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
    fn cancel(&self) {
        let _ = self.state.fetch_update(Ordering::AcqRel, Ordering::Acquire, |s| {
            (s != CONSUMED).then_some(CANCELLED)
        });
    }
}
struct State {
    status: Status,
    pending: Option<Arc<Pending>>,
}
struct Shared {
    state: Mutex<State>,
}
impl Shared {
    fn stop(&self, error: Error) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(state.status, Status::Stopped(_)) {
            state.status = Status::Stopped(error);
        }
        if let Some(pending) = state.pending.take() {
            pending.cancel();
        }
    }
}

/// A bounded prompt producer, NOT a grant of observation or input authority.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}
impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalApprovalHandle([one terminal])")
    }
}
impl Handle {
    pub fn status(&self) -> Status {
        self.shared.state.lock().map_or(Status::Stopped(Error::Poisoned), |s| s.status)
    }
    /// Install one question. Replacement waits until the worker releases the old
    /// display slot; no prompt queue or catch-up burst is permitted.
    pub fn ask(&self, question: Question) -> Result<Ticket, Error> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|_| Error::Entropy)?;
        let challenge = u128::from_ne_bytes(bytes);
        if challenge == 0 {
            return Err(Error::Entropy);
        }
        self.ask_at(question, challenge, Instant::now())
    }
    fn ask_at(&self, question: Question, challenge: u128, now: Instant) -> Result<Ticket, Error> {
        let mut state = self.shared.state.lock().map_err(|_| Error::Poisoned)?;
        if state.status != Status::Ready {
            return Err(Error::Unavailable);
        }
        if state.pending.is_some() {
            return Err(Error::Busy);
        }
        let pending = Arc::new(Pending {
            question,
            challenge,
            until: now.checked_add(PROMPT_TIMEOUT).ok_or(Error::Expired)?,
            state: AtomicU8::new(WAITING),
        });
        state.pending = Some(pending.clone());
        Ok(Ticket { pending, shared: self.shared.clone() })
    }
}

/// Unique decision receipt. Dropping it invalidates a queued or delivered yes;
/// a clone of the producer can never consume this ticket or revive its lifetime.
pub struct Ticket {
    pending: Arc<Pending>,
    shared: Arc<Shared>,
}
impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalApprovalTicket([one-use])")
    }
}
impl Ticket {
    pub fn question(&self) -> Question {
        self.pending.question
    }
    /// None is still waiting; Some is consumed exactly once. The caller MUST
    /// still perform its original authority/view checks when applying Some(true).
    pub fn take_decision(&mut self) -> Result<Option<bool>, Error> {
        self.take_at(Instant::now())
    }
    fn take_at(&mut self, now: Instant) -> Result<Option<bool>, Error> {
        // Serialize native terminal loss/owner stop with receipt consumption.
        let state = self.shared.state.lock().map_err(|_| Error::Poisoned)?;
        if state.status != Status::Ready {
            self.pending.cancel();
            return Err(Error::Closed);
        }
        let current = self.pending.state_at(now);
        match current {
            WAITING => Ok(None),
            ALLOWED | DENIED => self.pending.state.compare_exchange(
                current, CONSUMED, Ordering::AcqRel, Ordering::Acquire,
            ).map(|_| Some(current == ALLOWED)).map_err(|_| Error::Closed),
            _ if now >= self.pending.until => Err(Error::Expired),
            _ => Err(Error::Closed),
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.pending.cancel();
    }
}
