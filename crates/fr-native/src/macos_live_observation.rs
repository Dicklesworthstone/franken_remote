//! Bounded observation publication from one original macOS viewing attempt.
//!
//! The native session supplies actual decoder receipts. The callback connects
//! them and one explicit snapshot handle to the authenticated local owner.
//! It grants no control, opens no file, and never substitutes receipt age for
//! source age. Attaching the handle copies no pixels and requests no capture.
use fr_client::robot::live::{self, Evidence, Observation, Phase};
use fr_client::robot::snapshot::Handle;
use fr_wire::display::Display;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Instant,
};

/// Native lifecycle evidence, carrying the original receipt's local timestamp.
#[derive(Clone)]
pub enum Event {
    Begin(u8),
    Authenticated,
    Approval,
    /// The original observer's bounded on-demand export channel. It must be
    /// bound explicitly by the local IPC owner, separately from metadata.
    Snapshot(Handle),
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
    /// Apply metadata to the same bounded observation slot served by local IPC.
    /// Duplicate receipts retain their original age in that shared owner.
    /// Snapshot attachment requires an explicit handler; metadata-only sinks
    /// cannot report a successful binding they never installed.
    pub fn apply(self, state: &mut Observation) -> Result<(), live::Error> {
        match self {
            Self::Begin(attempt) => state.begin(attempt),
            Self::Authenticated => state.authenticated(),
            Self::Approval => state.approval(),
            Self::Snapshot(_) => Err(live::Error::Order),
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
    snapshot_bound: Cell<bool>,
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
            snapshot_bound: Cell::new(false),
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
        self.0.snapshot_bound.set(false);
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

    /// Attach at most once, before serving this exact observer. Retirement and
    /// failed callbacks fence attachment just like frame receipts; a stale
    /// callback cannot replace a newer attempt's export owner.
    pub(crate) fn bind_snapshot(&self, handle: Handle) -> Result<(), ()> {
        self.shared.check(self.attempt)?;
        if self.shared.snapshot_bound.replace(true) {
            return Err(());
        }
        self.send(Event::Snapshot(handle))?;
        self.shared.check(self.attempt)
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
