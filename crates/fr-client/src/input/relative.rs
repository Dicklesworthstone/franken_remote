//! Reliable pointer-mode barriers and cumulative relative checkpoints.
//!
//! A mode result describes a host-local change, never an OS submission. The
//! host invalidates all old tickets at that change; input resumes only after a
//! later timely ticket arrives on the same ordered feedback stream.
use super::Error;
use fr_core::{
    input::{InputEvent, PointerMode},
    input_sequence::InputOutcome,
};

#[derive(Clone, Copy)]
pub(super) enum Pending {
    None,
    Mode { mode: PointerMode, epoch: u64 },
    Relative { previous: (i64, i64) },
}

pub(super) struct State {
    mode: PointerMode,
    epoch: u64,
    cumulative: (i64, i64),
    switching: bool,
    awaiting_ticket: bool,
}

impl State {
    pub(super) const fn new() -> Self {
        Self {
            mode: PointerMode::Absolute,
            epoch: 0,
            cumulative: (0, 0),
            switching: false,
            awaiting_ticket: false,
        }
    }

    pub(super) const fn pending(&self) -> bool {
        self.switching || self.awaiting_ticket
    }

    pub(super) fn ready(&self) -> Result<(), Error> {
        if self.pending() {
            Err(Error::Backpressure)
        } else {
            Ok(())
        }
    }

    pub(super) fn absolute(&self) -> Result<(), Error> {
        self.ready()?;
        if self.mode == PointerMode::Absolute {
            Ok(())
        } else {
            Err(Error::InvalidTransition)
        }
    }

    pub(super) fn mode(&self, mode: PointerMode) -> Result<InputEvent<'static>, Error> {
        self.ready()?;
        if self.mode == mode {
            return Err(Error::InvalidTransition);
        }
        let epoch = self.epoch.checked_add(1).ok_or(Error::InvalidTransition)?;
        Ok(InputEvent::Mode { mode, epoch })
    }

    pub(super) fn relative(&self, x: i32, y: i32) -> Result<InputEvent<'static>, Error> {
        self.ready()?;
        if self.mode != PointerMode::Relative || (x == 0 && y == 0) {
            return Err(Error::InvalidTransition);
        }
        let cumulative_x = self.cumulative.0.checked_add(i64::from(x));
        let cumulative_y = self.cumulative.1.checked_add(i64::from(y));
        let (Some(cumulative_x), Some(cumulative_y)) = (cumulative_x, cumulative_y) else {
            return Err(Error::InvalidTransition);
        };
        Ok(InputEvent::Relative {
            mode_epoch: self.epoch,
            cumulative_x,
            cumulative_y,
        })
    }

    /// Called only after successful encoding. Failed encoding consumes neither
    /// a mode epoch nor relative distance; unsent abandonment has its own undo.
    pub(super) fn encoded(&mut self, event: InputEvent<'_>) -> Pending {
        match event {
            InputEvent::Mode { mode, epoch } => {
                self.switching = true;
                Pending::Mode { mode, epoch }
            }
            InputEvent::Relative {
                cumulative_x,
                cumulative_y,
                ..
            } => {
                let previous = self.cumulative;
                self.cumulative = (cumulative_x, cumulative_y);
                Pending::Relative { previous }
            }
            _ => Pending::None,
        }
    }

    pub(super) fn result(&mut self, pending: Pending, outcome: InputOutcome) {
        if let Pending::Mode { mode, epoch } = pending
            && outcome == InputOutcome::AppliedLocally
        {
            self.mode = mode;
            self.epoch = epoch;
            self.cumulative = (0, 0);
            self.switching = false;
            self.awaiting_ticket = true;
        }
    }

    pub(super) fn ticket(&mut self) {
        // Tickets received before the mode result leave switching unchanged.
        self.awaiting_ticket = false;
    }

    pub(super) fn abandon(&mut self, pending: Pending) {
        match pending {
            Pending::Mode { .. } => self.switching = false,
            Pending::Relative { previous } => self.cumulative = previous,
            Pending::None => {}
        }
    }
}
