//! A bounded, lease-scoped client path from UI actions to actual FRD0 records
//! and terminal host receipts. Encoding consumes an identity: send those bytes
//! once through the bounded authenticated transport, or stop this owner. Never
//! regenerate an uncertain action with a fresh ticket. Only metadata is retained.
mod control;
pub mod held;
pub mod ticket;
pub mod viewport;
use fr_core::{
    ids::{InputTicketId, RemoteSessionId},
    input::{
        DesktopPoint, InputBounds, InputCredentials, InputEvent, InputRequest, InputView,
        KeyTransition, MAX_COMMITTED_TEXT_BYTES, PhysicalKey, PointerButton, ScrollUnit,
    },
    input_sequence::InputOutcome,
    input_submission::{Capabilities, Capability},
    limits::ProtocolLimits,
};
use fr_wire::{
    WireError,
    input::{InputDelivery, InputDirection, encode_input},
    input_result::{InputResult, ResultBinding, SequenceSpace, decode_input_result},
};

use std::sync::Arc;

pub const MAX_PENDING_ACTIONS: usize = 32;
/// CLIENT monotonic microseconds, never compared to a host ticket timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClientInstant(pub u64);
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub view_age_us: u64,
    pub receipt_timeout_us: u64,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            view_age_us: 250_000,
            receipt_timeout_us: 2_000_000,
        }
    }
}
/// Evidence from the renderer/source-verification path, NOT a heartbeat or
/// decode callback. `source_age_upper_us` includes transport/clock uncertainty
/// at `received_at`; delayed presentation cannot reset the observation's age.
/// Static pixels may receive a new qualified unchanged-source observation.
#[derive(Debug, Clone, Copy)]
pub struct PresentedObservation {
    pub session: RemoteSessionId,
    pub serial: u64,
    pub view: InputView,
    pub received_at: ClientInstant,
    pub source_age_upper_us: u64,
}
/// Absolute-pointer profile; relative-mode negotiation remains a separate slice.
/// This deliberately separates committed text from physical keyboard events.
#[derive(Clone, Copy)]
pub enum Action<'a> {
    Key {
        key: PhysicalKey,
        transition: KeyTransition,
    },
    Button {
        button: PointerButton,
        pressed: bool,
        position: DesktopPoint,
    },
    Scroll {
        position: DesktopPoint,
        x: i32,
        y: i32,
        unit: ScrollUnit,
    },
    Text(&'a str),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    FocusLost,
    Suspended,
    Disconnected,
    ViewChanged,
    ViewStale,
    ClockRegression,
    CounterExhausted,
    ReceiptTimeout,
    ActionFailed,
    InvalidReceipt,
    InvalidTicket,
    InvalidControl,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    Stopped(StopReason),
    MappingUnconfirmed,
    NoPresentedView,
    ObsoleteObservation,
    StaleSession,
    Unsupported,
    OutOfBounds,
    InvalidTransition,
    Backpressure,
    TicketExpired,
    /// A stale or unknown view suspends input (plan 11.3). The action is
    /// refused, not queued; input resumes after fresh evidence and a new ticket.
    ViewSuspended,
    /// A release or repeat of a key or button that a suspension already
    /// released on the host. Nothing is sent; the caller drops the event.
    ReleasedBySuspension,
    Control(crate::authority::Error),
    Wire(WireError),
}
/// A record encoded but never admitted to the transport when a stale view
/// suspended input, identified by the sequence space it consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsent {
    Action(u64),
    Pointer,
    Held(u64),
}
/// Longest a stale view may suspend input before the grant ends with
/// `StopReason::ViewStale` (plan 11.3), matching the host's own limit.
pub const MAX_VIEW_SUSPENSION_US: u64 = 10_000_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct Encoded {
    pub bytes: usize,
    pub sequence: u64,
    pub space: SequenceSpace,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum ResultEvent {
    Completed(InputResult),
    Duplicate(InputResult),
    Unretained,
    Pointer(InputResult),
}
#[derive(Clone, Copy)]
pub(crate) struct Pending {
    sequence: u64,
    pub(crate) deadline: ClientInstant,
    minimum: u32,
    maximum: u32,
}
/// Non-cloneable owner for ONE explicit input grant. No method resumes a stopped
/// owner or rebinds its session/lease. The session coordinator must require a new
/// host grant after reconnect, focus loss, stale view, mapping change or failure.
/// Stop fences new sends; notify the host's independent revoke/cleanup path too.
/// A missing result is uncertainty, never a claim that the host did nothing.
pub struct InputClient {
    pub(crate) clipboard_projection: Option<crate::clipboard::Owner>,
    viewport_owner: Arc<()>,
    credentials: InputCredentials,
    binding: ResultBinding,
    bounds: InputBounds,
    capabilities: Capabilities,
    pub(crate) limits: ProtocolLimits,
    policy: Policy,
    /// The path's current source-age bound: `policy.view_age_us` is its base.
    view_bound_us: u64,
    clock: ClientInstant,
    started: ClientInstant,
    stopped: Option<StopReason>,
    pub(crate) mapped: bool,
    observation: Option<u64>,
    pub(crate) view_until: Option<ClientInstant>,
    next_action: Option<u64>,
    next_pointer: Option<u64>,
    next_held: Option<u64>,
    held_after: Option<ClientInstant>,
    pub(crate) pending: [Option<Pending>; MAX_PENDING_ACTIONS],
    receipts: [Option<InputResult>; MAX_PENDING_ACTIONS],
    receipt_cursor: usize,
    ticket_state: Option<ticket::State>,
    control_response: Option<crate::authority::ObservationResponder>,
    keys: [bool; 256],
    buttons: [bool; 5],
    /// When a stale view suspended input, if it is suspended now.
    suspended_since: Option<ClientInstant>,
    /// Keys and buttons held when a suspension released them on the host,
    /// until the user's own release or next press.
    released_keys: [bool; 256],
    released_buttons: [bool; 5],
    /// A suspension resumes only with a ticket issued after the host's lapse.
    awaiting_ticket: bool,
    suspensions: u32,
    suspended_us: u64,
}
impl InputClient {
    /// The caller has authenticated the channel and received an explicit grant.
    /// Capabilities/bounds are those granted, not merely requested by the viewer.
    pub fn new(
        credentials: InputCredentials,
        channel: u32,
        bounds: InputBounds,
        capabilities: Capabilities,
        limits: ProtocolLimits,
        policy: Policy,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        if channel == 0
            || credentials.session.as_raw() == 0
            || credentials.lease.as_raw() == 0
            || credentials.ticket.as_raw() == 0
            || !(1..=1_500_000).contains(&policy.view_age_us)
            || !(1..=10_000_000).contains(&policy.receipt_timeout_us)
        {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self {
            clipboard_projection: None,
            viewport_owner: Arc::new(()),
            credentials,
            binding: ResultBinding {
                channel,
                session: credentials.session,
                lease: credentials.lease,
            },
            bounds,
            capabilities,
            limits,
            policy,
            view_bound_us: policy.view_age_us,
            clock: now,
            started: now,
            stopped: None,
            mapped: false,
            observation: None,
            view_until: None,
            next_action: Some(0),
            next_pointer: Some(0),
            next_held: Some(0),
            held_after: None,
            pending: [None; MAX_PENDING_ACTIONS],
            receipts: [None; MAX_PENDING_ACTIONS],
            receipt_cursor: 0,
            ticket_state: None,
            control_response: None,
            keys: [false; 256],
            buttons: [false; 5],
            suspended_since: None,
            released_keys: [false; 256],
            released_buttons: [false; 5],
            awaiting_ticket: false,
            suspensions: 0,
            suspended_us: 0,
        })
    }
    pub fn stop(&mut self, reason: StopReason) {
        self.stopped.get_or_insert(reason);
        if let Some(owner) = &self.clipboard_projection {
            owner.stop();
        }
        if let Some(response) = &mut self.control_response {
            response.stop();
        }
    }
    pub const fn stopped(&self) -> Option<StopReason> {
        self.stopped
    }
    pub fn pending_actions(&self) -> usize {
        self.pending.iter().flatten().count()
    }
    fn fail<T>(&mut self, reason: StopReason) -> Result<T, Error> {
        self.stop(reason);
        Err(Error::Stopped(self.stopped.expect("just stopped")))
    }
    /// Service on idle as well as before UI actions. Receipts arriving after a
    /// timeout may still be collected, but never restore input authority.
    pub fn tick(&mut self, now: ClientInstant) -> Result<(), Error> {
        if let Some(reason) = self.stopped {
            return Err(Error::Stopped(reason));
        }
        if now < self.clock {
            return self.fail(StopReason::ClockRegression);
        }
        self.clock = now;
        if let Some(response) = &mut self.control_response
            && let Err(error) = response.tick(now)
        {
            self.stop(StopReason::InvalidControl);
            return Err(Error::Control(error));
        }
        if self.pending.iter().flatten().any(|p| now >= p.deadline) {
            return self.fail(StopReason::ReceiptTimeout);
        }
        if self.view_until.is_some_and(|until| now >= until) {
            self.suspend(now);
        }
        if self
            .suspended_since
            .is_some_and(|since| now.0.saturating_sub(since.0) >= MAX_VIEW_SUSPENSION_US)
        {
            return self.fail(StopReason::ViewStale);
        }
        if let Some(owner) = &self.clipboard_projection
            && owner.check(now).is_err()
        {
            return self.fail(StopReason::InvalidControl);
        }
        Ok(())
    }
    /// Call only on the host's authenticated acknowledgement of this exact map.
    pub fn confirm_mapping(
        &mut self,
        session: RemoteSessionId,
        view: InputView,
        now: ClientInstant,
    ) -> Result<(), Error> {
        if session != self.credentials.session {
            return Err(Error::StaleSession);
        }
        self.tick(now)?;
        if view != self.credentials.view {
            return self.fail(StopReason::ViewChanged);
        }
        self.mapped = true;
        self.clipboard_readiness();
        Ok(())
    }
    pub fn presented(
        &mut self,
        evidence: PresentedObservation,
        now: ClientInstant,
    ) -> Result<(), Error> {
        if evidence.session != self.credentials.session {
            return Err(Error::StaleSession);
        }
        self.tick(now)?;
        if evidence.received_at < self.started {
            return Err(Error::ObsoleteObservation);
        }
        if evidence.view != self.credentials.view {
            return self.fail(StopReason::ViewChanged);
        }
        if self
            .observation
            .is_some_and(|serial| evidence.serial <= serial)
        {
            return Err(Error::ObsoleteObservation);
        }
        if evidence.received_at > now {
            return self.fail(StopReason::ClockRegression);
        }
        let Some(remaining) = self.view_bound_us.checked_sub(evidence.source_age_upper_us) else {
            self.observation = Some(evidence.serial);
            self.suspend(now);
            return Ok(());
        };
        let Some(until) = evidence
            .received_at
            .0
            .checked_add(remaining)
            .map(ClientInstant)
        else {
            return self.fail(StopReason::CounterExhausted);
        };
        self.observation = Some(evidence.serial);
        if now >= until {
            self.suspend(now);
            return Ok(());
        }
        self.view_until = Some(until);
        self.try_resume(now);
        self.clipboard_readiness();
        Ok(())
    }
    /// A stale or unknown view suspends input (plan 11.3): nothing is encoded
    /// or queued until fresh evidence AND a ticket issued after the host's own
    /// lapse arrive. The host releases remotely held keys and buttons at that
    /// lapse, so the local held-state mirror starts empty again.
    pub fn suspend(&mut self, now: ClientInstant) {
        self.view_until = None;
        if self.suspended_since.is_none() && self.stopped.is_none() {
            self.suspended_since = Some(now);
            self.awaiting_ticket = true;
            self.suspensions = self.suspensions.saturating_add(1);
            // The clipboard lane's authority projection cannot outlive a
            // lapse: only a ticket proves the lease, none is issued while
            // suspended, and an expired projection is never revived. The
            // clipboard ends for this grant; input itself continues.
            if let Some(owner) = self.clipboard_projection.take() {
                owner.stop();
            }
            for (released, held) in self.released_keys.iter_mut().zip(self.keys) {
                *released |= held;
            }
            for (released, held) in self.released_buttons.iter_mut().zip(self.buttons) {
                *released |= held;
            }
            self.keys = [false; 256];
            self.buttons = [false; 5];
        }
    }
    fn try_resume(&mut self, now: ClientInstant) {
        if let Some(since) = self.suspended_since
            && !self.awaiting_ticket
            && self.view_until.is_some()
        {
            self.suspended_us = self
                .suspended_us
                .saturating_add(now.0.saturating_sub(since.0));
            self.suspended_since = None;
        }
    }
    /// While suspended, return the most recent record the transport never
    /// admitted (still in the caller's one unsent slot). Its sequence is reused
    /// by the next record, so the host's strictly ordered stream sees no gap.
    /// The host never saw it: nothing is replayed, retried or rolled back.
    pub fn abandon_unsent(&mut self, unsent: Unsent) -> Result<(), Error> {
        if self.suspended_since.is_none() {
            return Err(Error::InvalidTransition);
        }
        match unsent {
            Unsent::Action(sequence) => {
                if self.next_action != sequence.checked_add(1) {
                    return Err(Error::InvalidTransition);
                }
                let slot = self
                    .pending
                    .iter()
                    .position(|p| p.is_some_and(|p| p.sequence == sequence))
                    .ok_or(Error::InvalidTransition)?;
                self.pending[slot] = None;
                self.next_action = Some(sequence);
            }
            Unsent::Held(sequence) => {
                if self.next_held != sequence.checked_add(1) {
                    return Err(Error::InvalidTransition);
                }
                self.next_held = Some(sequence);
                self.held_after = None;
            }
            // Replaceable pointer state: the host tolerates missing datagrams.
            Unsent::Pointer => {}
        }
        Ok(())
    }
    /// Since when input is suspended by a stale view, if it is now.
    pub const fn suspended_since(&self) -> Option<ClientInstant> {
        self.suspended_since
    }
    /// Suspensions so far and their total duration, the current one included.
    pub fn suspension_totals(&self, now: ClientInstant) -> (u32, u64) {
        let current = self
            .suspended_since
            .map_or(0, |since| now.0.saturating_sub(since.0));
        (self.suspensions, self.suspended_us.saturating_add(current))
    }
    /// The source-age bound for this path, between the configured base and
    /// 1.5 s. Applies to later evidence; a view deadline already derived from
    /// earlier evidence is neither extended nor shortened by it.
    pub fn set_view_bound(&mut self, bound_us: u64) -> Result<(), Error> {
        if !(self.policy.view_age_us..=1_500_000).contains(&bound_us) {
            return Err(Error::InvalidConfiguration);
        }
        self.view_bound_us = bound_us;
        Ok(())
    }
    pub const fn view_bound_us(&self) -> u64 {
        self.view_bound_us
    }
    /// Follow this path's measured round trip: later evidence is judged against
    /// `fr_wire::presented::source_age_bound_us` with the configured base and
    /// `VIEWER_RTT_MULTIPLE`. Unknown RTT keeps the base. Returns the bound.
    pub fn follow_path_rtt(&mut self, smoothed_rtt_us: Option<u64>) -> Result<u64, Error> {
        let bound = fr_wire::presented::source_age_bound_us(
            self.policy.view_age_us,
            smoothed_rtt_us,
            fr_wire::presented::VIEWER_RTT_MULTIPLE,
        );
        self.set_view_bound(bound)?;
        Ok(bound)
    }
    /// A new opaque host ticket affects ONLY future actions. It neither changes
    /// pending action identities nor reopens stopped or unready control.
    pub fn ticket(&mut self, ticket: InputTicketId, now: ClientInstant) -> Result<(), Error> {
        self.tick(now)?;
        if self.ticket_state.is_some() {
            return self.fail(StopReason::InvalidTicket);
        }
        if ticket.as_raw() == 0 {
            return Err(Error::InvalidConfiguration);
        }
        self.credentials.ticket = ticket;
        Ok(())
    }
    fn ready(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.tick(now)?;
        if self.suspended_since.is_some() {
            return Err(Error::ViewSuspended);
        }
        if self.ticket_state.is_some_and(|s| now.0 >= s.until_us) {
            return Err(Error::TicketExpired);
        }
        if !self.mapped {
            return Err(Error::MappingUnconfirmed);
        }
        if self.view_until.is_none() {
            return Err(Error::NoPresentedView);
        }
        Ok(())
    }
    fn require(&self, cap: Capability) -> Result<(), Error> {
        if self.capabilities.contains(cap) {
            Ok(())
        } else {
            Err(Error::Unsupported)
        }
    }
    fn position(&self, p: DesktopPoint) -> Result<(), Error> {
        self.require(Capability::Absolute)?;
        if self.bounds.contains(p) {
            Ok(())
        } else {
            Err(Error::OutOfBounds)
        }
    }
    /// Replaceable pointer states do not consume reliable action sequence space
    /// or a receipt slot. The containing transport must bound/coalesce datagrams.
    pub fn pointer(
        &mut self,
        position: DesktopPoint,
        out: &mut [u8],
        now: ClientInstant,
    ) -> Result<Encoded, Error> {
        self.ready(now)?;
        self.position(position)?;
        let Some(sequence) = self.next_pointer else {
            return self.fail(StopReason::CounterExhausted);
        };
        let bytes = encode_input(
            InputRequest {
                credentials: self.credentials,
                sequence,
                event: InputEvent::Pointer { position },
            },
            out,
            &self.limits,
            self.binding.channel,
            InputDirection::ViewerToHost,
            InputDelivery::Datagram,
        )
        .map_err(Error::Wire)?;
        self.next_pointer = sequence.checked_add(1);
        Ok(Encoded {
            bytes,
            sequence,
            space: SequenceSpace::Pointer,
        })
    }
    /// No payload is retained; the fixed pending window accounts for results.
    /// An encoding failure consumes nothing. Successful encoding is a possible
    /// send, not OS submission: do not transparently retry after transport error.
    pub fn action(
        &mut self,
        action: Action<'_>,
        out: &mut [u8],
        now: ClientInstant,
    ) -> Result<Encoded, Error> {
        self.ready(now)?;
        let Some(slot) = self.pending.iter().position(Option::is_none) else {
            return Err(Error::Backpressure);
        };
        let Some(sequence) = self.next_action else {
            return self.fail(StopReason::CounterExhausted);
        };
        let coordinate = matches!(action, Action::Button { .. } | Action::Scroll { .. });
        let barrier = if coordinate {
            let Some(next) = self.next_pointer else {
                return self.fail(StopReason::CounterExhausted);
            };
            next
        } else {
            0
        };
        let (event, minimum, maximum) = self.event(action, barrier)?;
        let Some(deadline) = now
            .0
            .checked_add(self.policy.receipt_timeout_us)
            .map(ClientInstant)
        else {
            return self.fail(StopReason::CounterExhausted);
        };
        let bytes = encode_input(
            InputRequest {
                credentials: self.credentials,
                sequence,
                event,
            },
            out,
            &self.limits,
            self.binding.channel,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        match event {
            InputEvent::Key { key, transition } => {
                self.keys[usize::from(key.usage())] = transition != KeyTransition::Release;
                self.released_keys[usize::from(key.usage())] = false;
            }
            InputEvent::Button {
                button, pressed, ..
            } => {
                self.buttons[button as usize - 1] = pressed;
                self.released_buttons[button as usize - 1] = false;
            }
            _ => {}
        }
        self.pending[slot] = Some(Pending {
            sequence,
            deadline,
            minimum,
            maximum,
        });
        self.clipboard_readiness();
        self.next_action = sequence.checked_add(1);
        if coordinate {
            self.next_pointer = barrier.checked_add(1);
        }
        Ok(Encoded {
            bytes,
            sequence,
            space: SequenceSpace::Action,
        })
    }
    fn event<'a>(
        &self,
        action: Action<'a>,
        barrier: u64,
    ) -> Result<(InputEvent<'a>, u32, u32), Error> {
        Ok(match action {
            Action::Key { key, transition } => {
                self.require(Capability::Keys)?;
                let held = self.keys[usize::from(key.usage())];
                if transition != KeyTransition::Press
                    && !held
                    && self.released_keys[usize::from(key.usage())]
                {
                    return Err(Error::ReleasedBySuspension);
                }
                if (transition == KeyTransition::Press) == held {
                    return Err(Error::InvalidTransition);
                }
                let maximum = if transition == KeyTransition::Repeat {
                    self.require(Capability::Repeat)?;
                    2
                } else {
                    1
                };
                (InputEvent::Key { key, transition }, 1, maximum)
            }
            Action::Button {
                button,
                pressed,
                position,
            } => {
                self.require(Capability::Buttons)?;
                self.position(position)?;
                if !pressed
                    && !self.buttons[button as usize - 1]
                    && self.released_buttons[button as usize - 1]
                {
                    return Err(Error::ReleasedBySuspension);
                }
                if self.buttons[button as usize - 1] == pressed {
                    return Err(Error::InvalidTransition);
                }
                (
                    InputEvent::Button {
                        button,
                        pressed,
                        position,
                        barrier,
                    },
                    2,
                    2,
                )
            }
            Action::Scroll {
                position,
                x,
                y,
                unit,
            } => {
                self.position(position)?;
                self.require(match unit {
                    ScrollUnit::Pixels => Capability::PixelScroll,
                    ScrollUnit::Lines => Capability::LineScroll,
                })?;
                let compound = (unit == ScrollUnit::Lines)
                    .then(|| fr_core::input_submission::scroll::LineScroll::new(x, y))
                    .flatten()
                    .map_or(
                        2,
                        fr_core::input_submission::scroll::LineScroll::native_operations,
                    );
                (
                    InputEvent::Scroll {
                        position,
                        x,
                        y,
                        unit,
                        barrier,
                    },
                    2.min(compound),
                    2.max(compound),
                )
            }
            Action::Text(text) => {
                self.require(Capability::Text)?;
                if text.is_empty() || text.len() > MAX_COMMITTED_TEXT_BYTES {
                    return Err(Error::Unsupported);
                }
                let count = u32::try_from(text.chars().count()).map_err(|_| Error::Unsupported)?;
                (InputEvent::Text(text), count, count)
            }
        })
    }
    /// Preserve terminal receipts even during closing drain. Replayed receipts
    /// cannot free a new slot or re-enable input. A valid failed/partial/unknown
    /// result stops future actions but retains its confirmed external prefix.
    pub fn result(&mut self, bytes: &[u8], now: ClientInstant) -> Result<ResultEvent, Error> {
        let result = decode_input_result(
            bytes,
            &self.limits,
            self.binding,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        let _ = self.tick(now);
        if result.space == SequenceSpace::Pointer {
            if self
                .next_pointer
                .is_some_and(|next| result.sequence >= next)
            {
                return self.fail(StopReason::InvalidReceipt);
            }
            // A host pointer failure also fences its input owner. Never keep
            // issuing keys just because this result is in a separate space.
            if result.outcome != InputOutcome::SubmittedToOs {
                self.stop(StopReason::ActionFailed);
            }
            // Never acknowledges a reliable action with the same number.
            return Ok(ResultEvent::Pointer(result));
        }
        if let Some(old) = self
            .receipts
            .iter()
            .flatten()
            .find(|r| r.sequence == result.sequence)
        {
            if *old != result {
                return self.fail(StopReason::InvalidReceipt);
            }
            return Ok(ResultEvent::Duplicate(result));
        }
        let Some(slot) = self
            .pending
            .iter()
            .position(|p| p.is_some_and(|p| p.sequence == result.sequence))
        else {
            if self.next_action.is_none_or(|next| result.sequence < next) {
                return Ok(ResultEvent::Unretained);
            }
            return self.fail(StopReason::InvalidReceipt);
        };
        let p = self.pending[slot].expect("matched pending");
        let count = result.submitted_operations;
        let valid = match result.outcome {
            InputOutcome::SubmittedToOs => count == p.minimum || count == p.maximum,
            InputOutcome::PartiallySubmittedToOs | InputOutcome::EffectUnknown => count < p.maximum,
            InputOutcome::AppliedLocally => false,
            _ => count == 0,
        };
        if !valid {
            return self.fail(StopReason::InvalidReceipt);
        }
        self.pending[slot] = None;
        self.clipboard_readiness();
        self.receipts[self.receipt_cursor] = Some(result);
        self.receipt_cursor = (self.receipt_cursor + 1) % MAX_PENDING_ACTIONS;
        if result.outcome != InputOutcome::SubmittedToOs {
            self.stop(StopReason::ActionFailed);
        }
        Ok(ResultEvent::Completed(result))
    }
}

#[cfg(test)]
mod tests;

/// Input whose freshness evidence comes from the media presentation path.
pub mod presentation;
