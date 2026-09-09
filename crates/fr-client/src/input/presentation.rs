//! Joins actual media completion/source evidence to the existing input client.
//! There is no API here to supply a guessed source age or refresh input with a
//! heartbeat. The session still authenticates clock samples/channel bindings,
//! and the platform supplies qualified visibility (not just decode completion).
use super::{
    Action, ClientInstant, Encoded, InputClient, PresentedObservation, ResultEvent, StopReason,
};
use fr_core::{
    ids::InputTicketId,
    ids::RemoteSessionId,
    input::{DesktopPoint, InputView},
};
use fr_media::{
    delivery::{DecodedFrame, ReceivePipeline},
    freshness::{self, ClockCorrelation, ViewEvidence, ViewTracker},
};
use fr_wire::MediaLimits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Input(super::Error),
    Media(freshness::Error),
    ViewMismatch,
    AlreadyUsed,
}
impl From<super::Error> for Error {
    fn from(value: super::Error) -> Self {
        Self::Input(value)
    }
}
impl From<freshness::Error> for Error {
    fn from(value: freshness::Error) -> Self {
        Self::Media(value)
    }
}

/// One non-cloneable grant joined to one receiver/configuration. Every action
/// rechecks current source evidence and receiver lifetime before producing bytes.
/// On any terminal stop the enclosing session must signal the host's independent
/// revoke path. This type emits no network traffic and never invents a new grant.
pub struct PresentedInput {
    input: InputClient,
    view: ViewTracker,
    delivered_serial: Option<u64>,
    active: bool,
}
impl PresentedInput {
    pub fn new(
        mut input: InputClient,
        receiver: &ReceivePipeline,
        clock: ClockCorrelation,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        input.tick(now)?;
        if input.observation.is_some()
            || input.pending_actions() != 0
            || input.next_action != Some(0)
            || input.next_pointer != Some(0)
        {
            return Err(Error::AlreadyUsed);
        }
        let view = ViewTracker::new(receiver, clock, input.policy.view_age_us, now.0)?;
        if view.epoch().configuration != input.credentials.view.configuration
            || view.epoch().recovery != input.credentials.view.recovery
        {
            return Err(Error::ViewMismatch);
        }
        Ok(Self {
            input,
            view,
            delivered_serial: None,
            active: false,
        })
    }
    pub fn stopped(&self) -> Option<StopReason> {
        self.input.stopped()
    }
    pub fn pending_actions(&self) -> usize {
        self.input.pending_actions()
    }
    pub fn stop(&mut self, reason: StopReason) {
        self.view.hide();
        self.input.stop(reason);
    }
    /// Hidden/background/focus-lost views cannot retain this grant. Reopening a
    /// window is not reacquisition; late callbacks do not resurrect this owner.
    pub fn hidden(&mut self) {
        self.stop(StopReason::FocusLost);
    }
    pub fn confirm_mapping(
        &mut self,
        session: RemoteSessionId,
        view: InputView,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.input
            .confirm_mapping(session, view, now)
            .map_err(Error::Input)
    }
    pub fn ticket(&mut self, ticket: InputTicketId, now: ClientInstant) -> Result<(), Error> {
        self.input.ticket(ticket, now).map_err(Error::Input)
    }
    pub fn synchronize(
        &mut self,
        clock: ClockCorrelation,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.input.tick(now)?;
        self.view.synchronize(clock, now.0).map_err(Error::Media)
    }
    /// Actual, bound `MediaProgress` only. Unknown source state stops active input
    /// even when the old pixel timestamp would otherwise still be within budget.
    pub fn progress(
        &mut self,
        bytes: &[u8],
        limits: &MediaLimits,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.input.tick(now)?;
        if let Err(error) = self.view.progress(bytes, limits, now.0) {
            if self.view.is_closed() {
                self.input.stop(StopReason::ViewChanged);
            }
            return Err(Error::Media(error));
        }
        self.tick(now).map(|_| ())
    }
    pub fn decoded(
        &mut self,
        frame: DecodedFrame,
        submitted: bool,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.input.tick(now)?;
        self.view
            .decoded(frame, submitted, now.0)
            .map_err(Error::Media)
    }
    /// Invoke only after platform-qualified visibility of this exact frame.
    /// The canonical native presenter supplies the preceding decode token.
    pub fn visible(&mut self, frame: u64, now: ClientInstant) -> Result<ViewEvidence, Error> {
        self.input.tick(now)?;
        let evidence = self.view.visible(frame, now.0)?;
        self.deliver(evidence, now)?;
        Ok(evidence)
    }
    fn deliver(&mut self, evidence: ViewEvidence, now: ClientInstant) -> Result<(), Error> {
        if evidence.epoch.configuration != self.input.credentials.view.configuration
            || evidence.epoch.recovery != self.input.credentials.view.recovery
        {
            self.stop(StopReason::ViewChanged);
            return Err(Error::ViewMismatch);
        }
        if self.delivered_serial != Some(evidence.serial) {
            self.input.presented(
                PresentedObservation {
                    session: self.input.credentials.session,
                    serial: evidence.serial,
                    view: self.input.credentials.view,
                    received_at: ClientInstant(evidence.observed_at_client_us),
                    source_age_upper_us: evidence.source_age_upper_us,
                },
                now,
            )?;
            self.delivered_serial = Some(evidence.serial);
            self.active = true;
        }
        Ok(())
    }
    /// Service on idle, not only on UI events. False means temporarily awaiting
    /// a qualified view and forbids new input. Existing view/receipt deadlines
    /// continue running while a new compositor submission awaits visibility.
    pub fn tick(&mut self, now: ClientInstant) -> Result<bool, Error> {
        self.input.tick(now)?;
        match self.view.evidence(now.0) {
            Ok(evidence) => {
                self.deliver(evidence, now)?;
                Ok(true)
            }
            Err(freshness::Error::NotSubmitted) => Ok(false),
            Err(freshness::Error::SourceUnknown | freshness::Error::SourceStale)
                if !self.active =>
            {
                Ok(false)
            }
            Err(error) => {
                self.stop(StopReason::ViewStale);
                Err(Error::Media(error))
            }
        }
    }
    pub fn pointer(
        &mut self,
        position: DesktopPoint,
        out: &mut [u8],
        now: ClientInstant,
    ) -> Result<Encoded, Error> {
        if !self.tick(now)? {
            return Err(Error::Input(super::Error::NoPresentedView));
        }
        self.input.pointer(position, out, now).map_err(Error::Input)
    }
    pub fn action(
        &mut self,
        action: Action<'_>,
        out: &mut [u8],
        now: ClientInstant,
    ) -> Result<Encoded, Error> {
        if !self.tick(now)? {
            return Err(Error::Input(super::Error::NoPresentedView));
        }
        self.input.action(action, out, now).map_err(Error::Input)
    }
    /// Drain real receipts even after view failure; confirmed effects are not
    /// rolled back by losing presentation and actions are never regenerated.
    pub fn result(&mut self, bytes: &[u8], now: ClientInstant) -> Result<ResultEvent, Error> {
        self.input.result(bytes, now).map_err(Error::Input)
    }
}
