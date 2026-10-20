//! Causal report acknowledgement on the original input owner. Preparation is
//! not admission, and a report from another reporter cannot replace this one.
use super::{ClientInstant, Error, InputClient, StopReason};
use fr_media::presented::{AdmittedCursor, QueuedReport};
use fr_wire::decoder::Binding;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Resume {
    Clear,
    NeedLossReport,
    AfterLoss(u64),
}

pub(super) struct State {
    reporter: Option<AdmittedCursor>,
    ceiling: u64,
    acknowledged: u64,
    resume: Resume,
}
impl State {
    fn new(suspended: bool) -> Self {
        Self {
            reporter: None,
            ceiling: 0,
            acknowledged: 0,
            resume: if suspended {
                Resume::NeedLossReport
            } else {
                Resume::Clear
            },
        }
    }
    pub(super) fn lapse(&mut self) {
        self.resume = Resume::NeedLossReport;
    }
    pub(super) fn reset_recovery(&mut self) {
        // The explicitly newer ticket view proves the recovery fence. A new
        // lapse within that generation still needs its own negative report.
        *self = Self::new(false);
    }
    pub(super) fn acknowledge(&mut self, sequence: u64) -> Result<bool, ()> {
        if self.reporter.is_none()
            || sequence == 0
            || sequence > self.ceiling
            || sequence < self.acknowledged
        {
            return Err(());
        }
        self.acknowledged = sequence;
        match self.resume {
            Resume::Clear => Ok(true),
            Resume::AfterLoss(loss) if sequence > loss => {
                self.resume = Resume::Clear;
                Ok(true)
            }
            Resume::NeedLossReport | Resume::AfterLoss(_) => Ok(false),
        }
    }
}

impl InputClient {
    /// Enable only after selecting the readiness-ticket profile on this grant's
    /// original feedback route. The runtime must bind its actual reporter before
    /// consuming those tickets. This does not renew authority or resume input.
    pub fn expect_readiness_feedback(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.tick(now)?;
        if self.ticket_state.is_none() {
            return Err(Error::InvalidConfiguration);
        }
        if self.readiness.is_none() {
            self.readiness = Some(State::new(self.suspended_since.is_some()));
        }
        Ok(())
    }

    fn matches_report_binding(&self, binding: Binding) -> bool {
        let view = self.credentials.view;
        binding.parent.remote_session == self.credentials.session
            && self
                .ticket_state
                .is_some_and(|ticket| ticket.boot == binding.parent.host_boot)
            && binding.geometry == view.geometry
            && binding.viewport == view.viewport
            && binding.configuration == view.configuration
            && binding.recovery == view.recovery
    }

    /// Bind the original reporter and import only reports it already admitted
    /// before observation-to-control promotion. Later cursor snapshots must
    /// agree with every receipt consumed below; they cannot skip a loss report.
    /// Only explicit reference recovery permits binding a successor reporter.
    pub fn bind_readiness_reporter(
        &mut self,
        cursor: AdmittedCursor,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.tick(now)?;
        if !self.matches_report_binding(cursor.binding()) {
            return self.fail(StopReason::InvalidTicket);
        }
        let Some(state) = &mut self.readiness else {
            return Err(Error::InvalidConfiguration);
        };
        if let Some(original) = &state.reporter {
            if !cursor.is_from(original) || cursor.sequence() != state.ceiling {
                return self.fail(StopReason::InvalidTicket);
            }
            return Ok(());
        }
        let last_was_negative = cursor.sequence() != 0 && cursor.visible().is_none();
        state.ceiling = cursor.sequence();
        state.reporter = Some(cursor);
        if last_was_negative {
            // The report can predate this grant's local lapse. Establish a
            // new barrier instead of treating old negative metadata as proof.
            // Recovery may already be suspended without a remaining view, so
            // the fresh-to-stale transition alone cannot establish this latch.
            state.lapse();
            self.awaiting_ticket = true;
            self.suspend(now);
        }
        Ok(())
    }

    /// The original reporter must retain a negative report for this lapse.
    /// A reserved sequence or prepared byte buffer does not satisfy this gate.
    pub fn needs_loss_report(&self) -> bool {
        self.stopped.is_none()
            && self.readiness.as_ref().is_some_and(|state| {
                state.resume == Resume::NeedLossReport && state.reporter.is_some()
            })
    }

    /// Called immediately after actual transport admission, never preparation
    /// or backpressure. Positive receipts only advance the known report ceiling.
    /// A negative receipt is the causal barrier for the current suspension.
    // Deliberately consume the admission token at this owner transition.
    #[allow(clippy::needless_pass_by_value)]
    pub fn report_admitted(
        &mut self,
        receipt: QueuedReport,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.tick(now)?;
        let Some(state) = &mut self.readiness else {
            return Err(Error::InvalidConfiguration);
        };
        if !state
            .reporter
            .as_ref()
            .is_some_and(|cursor| receipt.is_from(cursor))
            || state.ceiling.checked_add(1) != Some(receipt.sequence())
        {
            return self.fail(StopReason::InvalidTicket);
        }
        state.ceiling = receipt.sequence();
        if receipt.visible().is_none() {
            if state.resume != Resume::NeedLossReport {
                // The report service may discover ageing after the input turn
                // sampled a fresh view. Suspend that view immediately too.
                self.suspend(now);
            }
            let state = self.readiness.as_mut().expect("selected original owner");
            state.resume = Resume::AfterLoss(receipt.sequence());
            self.awaiting_ticket = true;
            self.clipboard_readiness();
        }
        Ok(())
    }
}
