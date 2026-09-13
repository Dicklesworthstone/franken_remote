//! Bounded OS-event handoff to the original controlled session. The UI thread
//! never borrows QUIC or holds an authority lock while making native calls.
use super::{ControlledViewer, ViewerControl, now};
use fr_client::input::{self, Action};
pub use fr_client::input::{
    ClientInstant,
    viewport::{Layout, LocalPoint, Located, PositionedAction},
};
use fr_core::input::{KeyTransition, PhysicalKey};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, TryLockError},
};

pub const MAX_EVENTS: usize = 64;
pub const MAX_EVENT_AGE_US: u64 = 100_000;

/// No Debug: physical keys, pointer coordinates and text are not diagnostics.
pub enum Event {
    Key {
        key: PhysicalKey,
        transition: KeyTransition,
    },
    Pointer(Located),
    Positioned {
        location: Located,
        action: PositionedAction,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    AlreadyAttached,
    Closed,
    Clock,
    Expired,
    Overflow,
    Unavailable,
}
struct Captured {
    event: Event,
    sampled: ClientInstant,
}
struct Queue {
    events: VecDeque<Captured>,
    last_sample: ClientInstant,
}
/// A single native event producer. Drop, focus loss, hiding, resize, suspend and
/// native failure must stop it. Stopping is lock-free and wakes cancellation of
/// an in-flight network operation; no subsequent focus gain can resume control.
/// Capture timestamps must use `clock`, not wall time or the host's clock.
pub struct Source {
    queue: Arc<Mutex<Queue>>,
    control: ViewerControl,
}
pub(super) struct Receiver {
    queue: Arc<Mutex<Queue>>,
    pending: Option<Captured>,
}
impl Source {
    pub fn clock(&self) -> Result<ClientInstant, Error> {
        if self.control.is_stopped() {
            return Err(Error::Closed);
        }
        now(&self.control.cx)
            .map(ClientInstant)
            .map_err(|_| Error::Clock)
    }
    pub fn stop(&self) {
        self.control.stop();
    }
    /// Submit an event stamped when sampled, never when dequeued after a stall.
    /// Adjacent motion replaces motion, but never crosses an action barrier.
    /// Failure is terminal: a dropped release cannot leave a live remote drag.
    pub fn push(&mut self, event: Event, sampled: ClientInstant) -> Result<(), Error> {
        let result = self.push_inner(event, sampled);
        if result.is_err() {
            self.stop();
        }
        result
    }
    fn push_inner(&mut self, event: Event, sampled: ClientInstant) -> Result<(), Error> {
        let current = self.clock()?;
        check_age(sampled, current)?;
        let mut queue = self.queue.lock().map_err(|_| Error::Unavailable)?;
        if self.control.is_stopped() {
            return Err(Error::Closed);
        }
        if sampled < queue.last_sample {
            return Err(Error::Clock);
        }
        if let Some(first) = queue.events.front() {
            check_age(first.sampled, current)?;
        }
        queue.last_sample = sampled;
        if matches!(event, Event::Pointer(_))
            && queue
                .events
                .back()
                .is_some_and(|e| matches!(e.event, Event::Pointer(_)))
        {
            queue.events.pop_back();
        }
        if queue.events.len() == MAX_EVENTS {
            return Err(Error::Overflow);
        }
        queue.events.push_back(Captured { event, sampled });
        Ok(())
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.stop();
    }
}
fn check_age(sampled: ClientInstant, current: ClientInstant) -> Result<(), Error> {
    let age = current.0.checked_sub(sampled.0).ok_or(Error::Clock)?;
    if age >= MAX_EVENT_AGE_US {
        Err(Error::Expired)
    } else {
        Ok(())
    }
}
impl Receiver {
    pub(super) fn check(&self, current: ClientInstant) -> Result<(), Error> {
        if let Some(pending) = &self.pending {
            check_age(pending.sampled, current)?;
        }
        match self.queue.try_lock() {
            Ok(queue) => {
                if let Some(first) = queue.events.front() {
                    check_age(first.sampled, current)?;
                }
                Ok(())
            }
            Err(TryLockError::WouldBlock) => Ok(()),
            Err(TryLockError::Poisoned(_)) => Err(Error::Unavailable),
        }
    }
    fn front(&mut self, current: ClientInstant) -> Result<Option<&Captured>, Error> {
        self.check(current)?;
        if self.pending.is_none() {
            match self.queue.try_lock() {
                Ok(mut queue) => self.pending = queue.events.pop_front(),
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Poisoned(_)) => return Err(Error::Unavailable),
            }
        }
        Ok(self.pending.as_ref())
    }
}
impl ControlledViewer {
    /// Attach exactly one local event source to THIS already granted viewer.
    /// `drive` and `StreamingViewer` consume it automatically; no second input
    /// path, grant, ticket, wire sequence or async runtime is created.
    pub fn capture_input(&mut self) -> Result<Source, super::Error> {
        let at = self.check()?;
        if self.events.is_some() {
            return Err(super::Error::Capture(Error::AlreadyAttached));
        }
        let mut events = VecDeque::new();
        events
            .try_reserve_exact(MAX_EVENTS)
            .map_err(|_| super::Error::Capture(Error::Unavailable))?;
        let queue = Arc::new(Mutex::new(Queue {
            events,
            last_sample: at,
        }));
        self.events = Some(Receiver {
            queue: queue.clone(),
            pending: None,
        });
        Ok(Source {
            queue,
            control: self.control(),
        })
    }
    pub(super) fn dispatch_captured(&mut self) -> Result<(), super::Error> {
        let Some(mut receiver) = self.events.take() else {
            return Ok(());
        };
        let result = self.dispatch_one(&mut receiver);
        self.events = Some(receiver);
        result
    }
    fn dispatch_one(&mut self, receiver: &mut Receiver) -> Result<(), super::Error> {
        let at = self.check_inner()?;
        let Some(captured) = receiver.front(at).map_err(super::Error::Capture)? else {
            return Ok(());
        };
        if self.pending_send() {
            return Ok(());
        }
        let result = match &captured.event {
            Event::Key { key, transition } => self.action(Action::Key {
                key: *key,
                transition: *transition,
            }),
            Event::Pointer(location) => self.pointer_in_view(location),
            Event::Positioned { location, action } => self.action_in_view(location, *action),
        };
        match result {
            Ok(_) => {
                // Native event age survives encoding AND transport backpressure.
                let deadline = captured
                    .sampled
                    .0
                    .checked_add(MAX_EVENT_AGE_US)
                    .ok_or(super::Error::Capture(Error::Clock))?;
                if let Some(pending) = &mut self.pending {
                    pending.until = pending.until.min(deadline);
                }
                receiver.pending = None;
                Ok(())
            }
            Err(
                super::Error::Backpressure
                | super::Error::View(input::presentation::Error::Input(input::Error::Backpressure)),
            ) => Ok(()),
            Err(super::Error::Viewport(input::viewport::Error::OutsideImage))
                if matches!(captured.event, Event::Pointer(_)) =>
            {
                receiver.pending = None;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}
