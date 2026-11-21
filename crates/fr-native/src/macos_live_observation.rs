//! Bounded metadata publication from one original macOS viewing attempt.
//!
//! The native session supplies actual decoder receipts. The callback connects
//! them to the existing authenticated local observation owner; it grants no
//! control, opens no file, and never substitutes receipt age for source age.
use fr_client::robot::live::{self, Evidence, Observation, Phase};
use fr_wire::display::Display;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Instant,
};

/// Native lifecycle evidence, carrying the original receipt's local timestamp.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Begin(u8),
    Authenticated,
    Approval,
    Ready {
        display: Display,
        serial: u64,
        evidence: Evidence,
        at: Instant,
    },
    Presented {
        serial: u64,
        evidence: Evidence,
        at: Instant,
    },
    Retire(Phase),
}

impl Event {
    /// Apply to the same bounded observation slot served by local IPC.
    /// Duplicate receipts retain their original age in that shared owner.
    pub fn apply(self, state: &mut Observation) -> Result<(), live::Error> {
        match self {
            Self::Begin(attempt) => state.begin(attempt),
            Self::Authenticated => state.authenticated(),
            Self::Approval => state.approval(),
            Self::Ready {
                display,
                serial,
                evidence,
                at,
            } => state.ready(display, serial, evidence, at),
            Self::Presented {
                serial,
                evidence,
                at,
            } => state.presented(serial, evidence, at),
            Self::Retire(phase) => {
                state.retire(phase);
                Ok(())
            }
        }
    }
}

type Sink = Box<dyn FnMut(Event) -> Result<(), ()>>;

struct Shared {
    sink: RefCell<Sink>,
    attempt: Cell<u8>,
    active: Cell<bool>,
    failed: Cell<bool>,
}

impl Shared {
    fn send(&self, event: Event) -> Result<(), ()> {
        let failed_before = self.failed.get();
        let result = if let Ok(mut sink) = self.sink.try_borrow_mut() {
            sink(event)
        } else {
            self.active.set(false);
            self.failed.set(true);
            return Err(());
        };
        // A sink may ignore a failed reentrant call. Propagate that original
        // failure after its borrow ends, so its outer Ok cannot revive a frame.
        // A later explicit retirement may still confirm that the slot cleared.
        if result.is_err() || (!failed_before && self.failed.get()) {
            self.fail();
            return Err(());
        }
        result
    }

    fn fail(&self) {
        // Fence BEFORE invoking the sink. Reentrancy or a failed callback can
        // never leave the original attempt eligible to publish another frame.
        self.active.set(false);
        self.failed.set(true);
        if let Ok(mut sink) = self.sink.try_borrow_mut() {
            let _ = sink(Event::Retire(Phase::Stopped));
        }
    }

    fn check(&self, attempt: u8) -> Result<(), ()> {
        if self.active.get() && !self.failed.get() && self.attempt.get() == attempt {
            Ok(())
        } else {
            Err(())
        }
    }
}

/// The invocation owns one sink and only its current attempt may publish.
pub(crate) struct Publisher(Rc<Shared>);

impl Publisher {
    pub(crate) fn new(sink: impl FnMut(Event) -> Result<(), ()> + 'static) -> Self {
        Self(Rc::new(Shared {
            sink: RefCell::new(Box::new(sink)),
            attempt: Cell::new(0),
            active: Cell::new(false),
            failed: Cell::new(false),
        }))
    }

    pub(crate) fn begin(&self, attempt: u8) -> Result<(), ()> {
        if self.0.failed.get()
            || self.0.active.get()
            || !(1..=32).contains(&attempt)
            || attempt <= self.0.attempt.get()
        {
            return Err(());
        }
        self.0.active.set(false);
        self.0.attempt.set(attempt);
        self.0.send(Event::Begin(attempt))?;
        self.0.active.set(true);
        Ok(())
    }

    pub(crate) fn authenticated(&self, attempt: u8) -> Result<(), ()> {
        self.0.check(attempt)?;
        self.0.send(Event::Authenticated)
    }

    pub(crate) fn original(&self, attempt: u8) -> Result<Attempt, ()> {
        self.0.check(attempt)?;
        Ok(Attempt {
            shared: Rc::clone(&self.0),
            attempt,
        })
    }

    pub(crate) fn retire(&self, phase: Phase) -> Result<(), ()> {
        self.0.active.set(false);
        self.0.send(Event::Retire(phase))
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        let _ = self.retire(Phase::Stopped);
    }
}

/// A callback is permanently scoped to its original attempt. A retained old
/// callback cannot clear, renew or replace a later attempt's observation.
#[derive(Clone)]
pub(crate) struct Attempt {
    shared: Rc<Shared>,
    attempt: u8,
}

impl Attempt {
    fn send(&self, event: Event) -> Result<(), ()> {
        self.shared.check(self.attempt)?;
        self.shared.send(event)
    }

    pub(crate) fn approval(&self) -> Result<(), ()> {
        self.send(Event::Approval)
    }

    pub(crate) fn ready(
        &self,
        display: Display,
        serial: u64,
        evidence: Evidence,
        at: Instant,
    ) -> Result<(), ()> {
        self.send(Event::Ready {
            display,
            serial,
            evidence,
            at,
        })
    }

    pub(crate) fn presented(&self, serial: u64, evidence: Evidence, at: Instant) -> Result<(), ()> {
        self.send(Event::Presented {
            serial,
            evidence,
            at,
        })
    }
}

#[cfg(test)]
#[path = "macos_live_observation/tests.rs"]
mod tests;
