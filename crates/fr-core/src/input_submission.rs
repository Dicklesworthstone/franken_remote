//! Serialized final input boundary. No OS call is authorized by parsing alone.
//! The caller supplies an already granted session and a qualified synchronous
//! sink. A watchdog must service `maintain` while idle and use the independent
//! revoke handle when OS work stalls. An OS call already entered is irreversible.
use crate::{
    authority::{AuthorityError, SessionAuthority},
    ids::{InputLeaseId, InputTicketId, RemoteSessionId},
    input::{
        DesktopPoint, InputBounds, InputCredentials, InputEvent, InputRequest, InputView,
        KeyTransition, PhysicalKey, PointerButton, PointerMode, ScrollUnit,
    },
    input_sequence::{
        InputAdmission, InputOutcome, InputSequenceError, InputSequenceLedger,
        MAX_RETAINED_INPUT_RECEIPTS,
    },
    time::HostInstant,
};
use core::fmt;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Explicitly qualified backend operations; absence is a refusal, not fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Capability {
    Keys,
    Repeat,
    Absolute,
    Buttons,
    Relative,
    PixelScroll,
    LineScroll,
    Text,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities(u16);
impl Capabilities {
    #[must_use]
    pub const fn with(self, capability: Capability) -> Self {
        Self(self.0 | (1 << capability as u8))
    }
    pub const fn contains(self, capability: Capability) -> bool {
        self.0 & (1 << capability as u8) != 0
    }
    pub const fn contains_all(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

/// One bounded native API operation. Text is one Unicode scalar, never half of
/// a surrogate pair. A button/scroll action first positions, then acts; each
/// operation has its own final clock check. Debug hides all input payloads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Key {
        key: PhysicalKey,
        transition: KeyTransition,
    },
    Absolute(DesktopPoint),
    Button {
        button: PointerButton,
        pressed: bool,
    },
    Relative {
        x: i32,
        y: i32,
    },
    Scroll {
        x: i32,
        y: i32,
        unit: ScrollUnit,
    },
    Text(char),
}
impl fmt::Debug for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Key { .. } => "Key",
            Self::Absolute(_) => "Absolute",
            Self::Button { .. } => "Button",
            Self::Relative { .. } => "Relative",
            Self::Scroll { .. } => "Scroll",
            Self::Text(_) => "Text",
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformError {
    Unsupported,
    Permission,
    GeometryChanged,
    Unavailable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Submission {
    Submitted,
    NotSubmitted(PlatformError),
    Unknown,
}

/// Implementations must not defer submission to another queue or perform
/// hidden retries. Potentially blocking preflight happens in `prepare`, before
/// the owner samples its final clock. `submit` performs one native operation;
/// its result names OS API submission, never application processing.
pub trait InputSink {
    fn prepare(&mut self, operation: Operation) -> Result<(), PlatformError>;
    fn submit(&mut self, operation: Operation) -> Submission;
    /// Undo reversible preparation when final authorization fails, on unwind,
    /// or after submission. Idempotent, release-only and never a new input event.
    /// Backends must retain uncertain restoration state for local cleanup.
    fn cancel_prepared(&mut self) {}
    /// X11 has no standalone repeat event. Request two separately authorized
    /// operations instead of hiding release/press inside one native submission.
    fn repeat_requires_pair(&self) -> bool {
        false
    }
}

struct PreparedSink<'a, S: InputSink>(&'a mut S);
impl<S: InputSink> Drop for PreparedSink<'_, S> {
    fn drop(&mut self) {
        self.0.cancel_prepared();
    }
}

/// Local-only one-way cancellation. This never grants or renews a lease and
/// needs no authority/codec mutex. It cannot undo a native call already entered.
#[derive(Clone)]
pub struct RevokeHandle(Arc<AtomicBool>);
impl RevokeHandle {
    pub fn revoke(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_revoked(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
/// Read/revoke-only access to the SAME authority used at native submission.
/// The monitor cannot grant control, create tickets, or renew a lease. Its
/// mutex protects only pure policy operations, never platform preparation,
/// submission, cleanup, or a caller-supplied clock callback.
#[derive(Clone)]
pub struct InputMonitor {
    authority: Arc<Mutex<SessionAuthority>>,
    revoke: RevokeHandle,
}
impl InputMonitor {
    pub fn revoke(&self) {
        self.revoke.revoke();
    }
    pub fn is_revoked(&self) -> bool {
        self.revoke.is_revoked()
    }
    /// Check a freshly sampled clock, returning the actual authority deadline.
    /// Ticket expiry is intentionally independent: it refuses new actions but
    /// does not release a key that is still held under a live control lease.
    pub fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        let result = self.with(|a| {
            let deadline = a.control_deadline()?;
            if now >= deadline {
                // Fence before releasing the policy lock. A concurrent renewal
                // cannot install authority after this terminal expiry decision.
                self.revoke();
                return Err(AuthorityError::LeaseExpired);
            }
            Ok(deadline)
        });
        if result.is_err() {
            self.revoke();
        }
        result
    }
    fn with<T>(
        &self,
        f: impl FnOnce(&mut SessionAuthority) -> Result<T, AuthorityError>,
    ) -> Result<T, Refusal> {
        if self.is_revoked() {
            return Err(Refusal::Revoked);
        }
        let mut authority = self.authority.lock().map_err(|_| {
            self.revoke();
            Refusal::AuthorityUnavailable
        })?;
        if self.is_revoked() {
            return Err(Refusal::Revoked);
        }
        f(&mut authority).map_err(Refusal::Authority)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Authority(AuthorityError),
    Sequence(InputSequenceError),
    StaleSession,
    StaleLease,
    StaleView,
    OutOfBounds,
    Unsupported,
    InvalidTransition,
    RelativeOverflow,
    ModeMismatch,
    Revoked,
    Platform(PlatformError),
    UnknownEffect,
    AuthorityUnavailable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub sequence: u64,
    pub outcome: InputOutcome,
    /// Confirmed native operations, not characters, clicks or semantic effects.
    pub submitted_operations: u32,
    pub refusal: Option<Refusal>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Dispatch {
    Completed(Receipt),
    ConsumedWithoutReceipt,
    ObsoletePointer,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct Cleanup {
    pub submitted_releases: u16,
    /// Rejected/unknown releases remain tracked for later local-only cleanup.
    pub remaining: u16,
}

/// Owns one already granted input lease, its replay ledger, and synthetic held
/// state. Never reconstruct it to resume an old lease. No Clone or secret Debug.
/// The enclosing OS share-session owner still arbitrates the global controller.
pub struct InputSession {
    authority: InputMonitor,
    session: RemoteSessionId,
    lease: InputLeaseId,
    view: InputView,
    bounds: InputBounds,
    capabilities: Capabilities,
    ledger: InputSequenceLedger,
    receipts: [Option<Receipt>; MAX_RETAINED_INPUT_RECEIPTS],
    receipt_cursor: usize,
    revoke: RevokeHandle,
    keys: [bool; 256],
    buttons: [bool; 5],
    pointer_floor: Option<u64>,
    mode: PointerMode,
    mode_epoch: u64,
    mode_ticket: Option<InputTicketId>,
    cumulative: (i64, i64),
}
impl InputSession {
    pub fn new(
        mut authority: SessionAuthority,
        credentials: InputCredentials,
        bounds: InputBounds,
        capabilities: Capabilities,
        now: HostInstant,
    ) -> Result<Self, Refusal> {
        if authority.session() != credentials.session {
            return Err(Refusal::StaleSession);
        }
        authority
            .authorize_submission(credentials.lease, credentials.ticket, now)
            .map_err(Refusal::Authority)?;
        let revoke = RevokeHandle(Arc::new(AtomicBool::new(false)));
        let authority = InputMonitor {
            authority: Arc::new(Mutex::new(authority)),
            revoke: revoke.clone(),
        };
        Ok(Self {
            authority,
            session: credentials.session,
            lease: credentials.lease,
            view: credentials.view,
            bounds,
            capabilities,
            ledger: InputSequenceLedger::new(credentials.lease, MAX_RETAINED_INPUT_RECEIPTS)
                .map_err(Refusal::Sequence)?,
            receipts: [None; MAX_RETAINED_INPUT_RECEIPTS],
            receipt_cursor: 0,
            revoke,
            keys: [false; 256],
            buttons: [false; 5],
            pointer_floor: None,
            mode: PointerMode::Absolute,
            mode_epoch: 0,
            mode_ticket: Some(credentials.ticket),
            cumulative: (0, 0),
        })
    }
    /// Immutable locally granted geometry for native factory validation.
    pub const fn bounds(&self) -> InputBounds {
        self.bounds
    }
    /// The negotiated operations, which may be a subset of native support.
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    pub fn monitor(&self) -> InputMonitor {
        self.authority.clone()
    }
    pub fn revoke_handle(&self) -> RevokeHandle {
        self.revoke.clone()
    }
    /// Fence first. Cleanup is explicit and may remain uncertain after a failed OS call.
    pub fn revoke(&mut self) {
        self.revoke.revoke();
        self.ledger.fence();

        self.mode_ticket = None;
    }
    /// Focus loss, view/configuration/mapping replacement and worker failure
    /// require a new grant. Restoring pixels never resurrects this input owner.
    pub fn invalidate_view(&mut self) {
        self.revoke();
    }
    pub fn suspend(&mut self) {
        self.revoke();
    }
    pub fn issue_observation_challenge(
        &mut self,
        nonce: u128,
        now: HostInstant,
    ) -> Result<HostInstant, Refusal> {
        self.check_active()?;
        self.authority
            .with(|a| a.issue_observation_challenge(nonce, now))
    }
    pub fn renew_observation(
        &mut self,
        nonce: u128,
        now: HostInstant,
    ) -> Result<HostInstant, Refusal> {
        self.check_active()?;
        self.authority
            .with(|a| a.respond_observation_challenge(nonce, now))
    }
    pub fn issue_control_challenge(
        &mut self,
        nonce: u128,
        now: HostInstant,
    ) -> Result<HostInstant, Refusal> {
        self.check_active()?;
        self.authority
            .with(|a| a.issue_control_challenge(nonce, now))
    }
    pub fn renew_control(&mut self, nonce: u128, now: HostInstant) -> Result<HostInstant, Refusal> {
        self.check_active()?;
        self.authority
            .with(|a| a.respond_control_challenge(self.lease, nonce, now))
    }
    /// The local authority supplies a fresh unpredictable ID. This is NOT an
    /// automatic retry API: consumed actions remain consumed after renewal.
    pub fn issue_ticket(
        &mut self,
        ticket: InputTicketId,
        now: HostInstant,
    ) -> Result<HostInstant, Refusal> {
        self.check_active()?;
        let until = self
            .authority
            .with(|a| a.issue_input_ticket(self.lease, ticket, now))?;
        self.mode_ticket = Some(ticket);
        Ok(until)
    }
    /// Service independently during idle, not only when a packet arrives.
    /// Ticket expiry rejects actions; lease/view expiry additionally ends held
    /// state. A runtime watchdog must call this; this type starts no timer itself.
    pub fn maintain(&mut self, now: HostInstant, sink: &mut impl InputSink) -> Cleanup {
        if self.revoke.is_revoked()
            || !self
                .authority
                .with(|a| Ok(a.has_live_control(now)))
                .unwrap_or(false)
        {
            self.revoke();
        }
        if self.revoke.is_revoked() {
            self.cleanup(sink)
        } else {
            Cleanup {
                submitted_releases: 0,
                remaining: self.held_count(),
            }
        }
    }
    /// Release-only, bounded local cleanup. No remote ticket can create presses
    /// here. Uncertain releases stay tracked. Local physical-key collisions are
    /// a platform trust limitation, not claimed to be perfectly attributable.
    pub fn cleanup(&mut self, sink: &mut impl InputSink) -> Cleanup {
        self.revoke();
        let mut submitted = 0;
        for index in 0..self.keys.len() {
            if self.keys[index] {
                let key = PhysicalKey::new(u16::try_from(index).expect("fixed key array"));
                if let Some(key) = key {
                    let op = Operation::Key {
                        key,
                        transition: KeyTransition::Release,
                    };
                    if sink.prepare(op).is_ok() && sink.submit(op) == Submission::Submitted {
                        self.keys[index] = false;
                        submitted += 1;
                    }
                }
            }
        }
        for (index, button) in [
            PointerButton::Primary,
            PointerButton::Secondary,
            PointerButton::Middle,
            PointerButton::Back,
            PointerButton::Forward,
        ]
        .into_iter()
        .enumerate()
        {
            if self.buttons[index] {
                let op = Operation::Button {
                    button,
                    pressed: false,
                };
                if sink.prepare(op).is_ok() && sink.submit(op) == Submission::Submitted {
                    self.buttons[index] = false;
                    submitted += 1;
                }
            }
        }
        Cleanup {
            submitted_releases: submitted,
            remaining: self.held_count(),
        }
    }
    /// Read an already recorded reliable result without admitting or retrying
    /// an action. Used by native supervision after an unwind to retain the
    /// confirmed operation prefix. Eviction remains explicit; no input content.
    pub fn retained_receipt(&self, sequence: u64) -> Option<Receipt> {
        self.receipts
            .iter()
            .flatten()
            .find(|r| r.sequence == sequence)
            .copied()
    }
    pub fn held_count(&self) -> u16 {
        u16::try_from(
            self.keys
                .iter()
                .chain(&self.buttons)
                .filter(|held| **held)
                .count(),
        )
        .expect("fixed held arrays")
    }
    pub fn dispatch(
        &mut self,
        request: InputRequest<'_>,
        sink: &mut impl InputSink,
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<Dispatch, Refusal> {
        if request.credentials.session != self.session {
            return Err(Refusal::StaleSession);
        }
        if request.credentials.lease != self.lease {
            return Err(Refusal::StaleLease);
        }
        let pointer = request.event.is_pointer();
        if pointer {
            if self
                .pointer_floor
                .is_some_and(|floor| request.sequence <= floor)
            {
                return Ok(Dispatch::ObsoletePointer);
            }
            self.check_active()?;
            self.pointer_floor = Some(request.sequence);
        } else {
            match self.ledger.admit(self.lease, request.sequence) {
                Ok(InputAdmission::Admitted) => {}
                Ok(InputAdmission::Completed(_)) => {
                    return Ok(self
                        .receipts
                        .iter()
                        .flatten()
                        .find(|r| r.sequence == request.sequence)
                        .copied()
                        .map_or(Dispatch::ConsumedWithoutReceipt, Dispatch::Completed));
                }
                Ok(InputAdmission::ConsumedWithoutReceipt | InputAdmission::InFlight) => {
                    return Ok(Dispatch::ConsumedWithoutReceipt);
                }
                Err(error) => {
                    if self.ledger.is_fenced() {
                        self.revoke();
                    }
                    return Err(Refusal::Sequence(error));
                }
            }
        }
        let mut attempt = Attempt {
            owner: self,
            request,
            pointer,
            submitted: 0,
            uncertain: false,
            completed: false,
        };
        let result = attempt.execute(sink, &mut clock);
        Ok(Dispatch::Completed(attempt.finish(result)))
    }
    fn check_active(&self) -> Result<(), Refusal> {
        if self.revoke.is_revoked() || self.ledger.is_fenced() {
            Err(Refusal::Revoked)
        } else {
            Ok(())
        }
    }
    fn check(&mut self, credentials: InputCredentials, now: HostInstant) -> Result<(), Refusal> {
        self.check_active()?;
        if credentials.view != self.view {
            return Err(Refusal::StaleView);
        }
        if self.mode_ticket != Some(credentials.ticket) {
            return Err(Refusal::Authority(AuthorityError::TicketInvalid));
        }
        self.authority
            .with(|a| a.authorize_submission(self.lease, credentials.ticket, now))
    }
    fn require(&self, cap: Capability) -> Result<(), Refusal> {
        if self.capabilities.contains(cap) {
            Ok(())
        } else {
            Err(Refusal::Unsupported)
        }
    }
    fn position(&self, position: DesktopPoint) -> Result<(), Refusal> {
        self.require(Capability::Absolute)?;
        if self.mode != PointerMode::Absolute {
            return Err(Refusal::ModeMismatch);
        }
        if !self.bounds.contains(position) {
            return Err(Refusal::OutOfBounds);
        }
        Ok(())
    }
    fn barrier(&mut self, barrier: u64) {
        self.pointer_floor = Some(self.pointer_floor.map_or(barrier, |old| old.max(barrier)));
    }
    fn record(&mut self, receipt: Receipt, pointer: bool) {
        if !pointer {
            let recorded =
                self.ledger
                    .record_outcome(self.lease, receipt.sequence, receipt.outcome);
            if recorded.is_err() {
                self.revoke();
            }
            self.receipts[self.receipt_cursor] = Some(receipt);
            self.receipt_cursor = (self.receipt_cursor + 1) % self.receipts.len();
        }
        if !matches!(
            receipt.outcome,
            InputOutcome::SubmittedToOs | InputOutcome::AppliedLocally
        ) {
            self.revoke();
        }
    }
}

impl Drop for InputSession {
    fn drop(&mut self) {
        // Monitors cannot outlive the submission owner as live authority.
        // Native release remains an explicit, separately acknowledged operation.
        self.revoke.revoke();
    }
}

struct Attempt<'a, 'event> {
    owner: &'a mut InputSession,
    request: InputRequest<'event>,
    pointer: bool,
    submitted: u32,
    uncertain: bool,
    completed: bool,
}
impl Attempt<'_, '_> {
    fn one(
        &mut self,
        op: Operation,
        sink: &mut impl InputSink,
        clock: &mut impl FnMut() -> HostInstant,
    ) -> Result<(), Refusal> {
        // Install the cleanup guard before prepare: even preparation can panic
        // after acquiring reversible platform state. No native press is allowed
        // in prepare, and a stale ticket must not leave that state stranded.
        let prepared = PreparedSink(sink);
        prepared.0.prepare(op).map_err(Refusal::Platform)?;
        self.owner.check(self.request.credentials, clock())?;
        // Track a possible press BEFORE invoking the platform. Even a panic or
        // an unknown native result must leave enough state for release cleanup.
        let previous = match op {
            Operation::Key {
                key,
                transition: KeyTransition::Press,
            } => Some(self.owner.keys[usize::from(key.usage())]),
            Operation::Button {
                button,
                pressed: true,
            } => Some(self.owner.buttons[button as usize - 1]),
            _ => None,
        };
        self.track(op, true);
        self.uncertain = true;
        match prepared.0.submit(op) {
            Submission::Submitted => {
                self.uncertain = false;
                self.submitted += 1;
                self.track(op, false);
                Ok(())
            }
            Submission::Unknown => Err(Refusal::UnknownEffect),
            Submission::NotSubmitted(error) => {
                self.uncertain = false;
                if let Some(old) = previous {
                    match op {
                        Operation::Key { key, .. } => {
                            self.owner.keys[usize::from(key.usage())] = old;
                        }
                        Operation::Button { button, .. } => {
                            self.owner.buttons[button as usize - 1] = old;
                        }
                        _ => {}
                    }
                }
                Err(Refusal::Platform(error))
            }
        }
    }
    fn track(&mut self, op: Operation, before: bool) {
        match op {
            Operation::Key {
                key,
                transition: KeyTransition::Press,
            } => self.owner.keys[usize::from(key.usage())] = true,
            Operation::Key {
                key,
                transition: KeyTransition::Release,
            } if !before => self.owner.keys[usize::from(key.usage())] = false,
            Operation::Button {
                button,
                pressed: true,
            } => self.owner.buttons[button as usize - 1] = true,
            Operation::Button {
                button,
                pressed: false,
            } if !before => self.owner.buttons[button as usize - 1] = false,
            _ => {}
        }
    }
    fn key_transition(
        &mut self,
        key: PhysicalKey,
        transition: KeyTransition,
        sink: &mut impl InputSink,
        clock: &mut impl FnMut() -> HostInstant,
    ) -> Result<(), Refusal> {
        self.owner.require(Capability::Keys)?;
        let held = self.owner.keys[usize::from(key.usage())];
        match transition {
            KeyTransition::Repeat => {
                self.owner.require(Capability::Repeat)?;
                if !held {
                    return Err(Refusal::InvalidTransition);
                }
            }
            KeyTransition::Press if held => return Err(Refusal::InvalidTransition),
            KeyTransition::Release if !held => return Err(Refusal::InvalidTransition),
            _ => {}
        }
        if transition == KeyTransition::Repeat && sink.repeat_requires_pair() {
            self.one(
                Operation::Key {
                    key,
                    transition: KeyTransition::Release,
                },
                sink,
                clock,
            )?;
            self.one(
                Operation::Key {
                    key,
                    transition: KeyTransition::Press,
                },
                sink,
                clock,
            )
        } else {
            self.one(Operation::Key { key, transition }, sink, clock)
        }
    }
    fn execute(
        &mut self,
        sink: &mut impl InputSink,
        clock: &mut impl FnMut() -> HostInstant,
    ) -> Result<(), Refusal> {
        self.owner.check(self.request.credentials, clock())?;
        match self.request.event {
            InputEvent::Key { key, transition } => {
                self.key_transition(key, transition, sink, clock)
            }
            InputEvent::Pointer { position } => {
                self.owner.position(position)?;
                self.one(Operation::Absolute(position), sink, clock)
            }
            InputEvent::Button {
                button,
                pressed,
                position,
                barrier,
            } => {
                self.owner.require(Capability::Buttons)?;
                self.owner.position(position)?;
                if self.owner.buttons[button as usize - 1] == pressed {
                    return Err(Refusal::InvalidTransition);
                }
                self.owner.barrier(barrier);
                self.one(Operation::Absolute(position), sink, clock)?;
                self.one(Operation::Button { button, pressed }, sink, clock)
            }
            InputEvent::Scroll {
                position,
                barrier,
                x,
                y,
                unit,
            } => {
                self.owner.require(match unit {
                    ScrollUnit::Pixels => Capability::PixelScroll,
                    ScrollUnit::Lines => Capability::LineScroll,
                })?;
                self.owner.position(position)?;
                self.owner.barrier(barrier);
                self.one(Operation::Absolute(position), sink, clock)?;
                self.one(Operation::Scroll { x, y, unit }, sink, clock)
            }
            InputEvent::Text(text) => {
                self.owner.require(Capability::Text)?;
                if text.is_empty() || text.len() > crate::input::MAX_COMMITTED_TEXT_BYTES {
                    return Err(Refusal::Unsupported);
                }
                for scalar in text.chars() {
                    self.one(Operation::Text(scalar), sink, clock)?;
                }
                Ok(())
            }
            InputEvent::Relative {
                mode_epoch,
                cumulative_x,
                cumulative_y,
            } => {
                self.owner.require(Capability::Relative)?;
                if self.owner.mode != PointerMode::Relative || mode_epoch != self.owner.mode_epoch {
                    return Err(Refusal::ModeMismatch);
                }
                let delta = |new: i64, old: i64| {
                    new.checked_sub(old)
                        .and_then(|n| i32::try_from(n).ok())
                        .ok_or(Refusal::RelativeOverflow)
                };
                let x = delta(cumulative_x, self.owner.cumulative.0)?;
                let y = delta(cumulative_y, self.owner.cumulative.1)?;
                if x != 0 || y != 0 {
                    self.one(Operation::Relative { x, y }, sink, clock)?;
                }
                self.owner.cumulative = (cumulative_x, cumulative_y);
                Ok(())
            }
            InputEvent::Mode { mode, epoch } => {
                self.owner.require(match mode {
                    PointerMode::Absolute => Capability::Absolute,
                    PointerMode::Relative => Capability::Relative,
                })?;
                if epoch <= self.owner.mode_epoch || self.owner.held_count() != 0 {
                    return Err(Refusal::InvalidTransition);
                }
                self.owner.mode = mode;
                self.owner.mode_epoch = epoch;
                self.owner.cumulative = (0, 0);
                // A new ticket binds the new mode. Old absolute datagrams do not
                // carry a mode epoch, so they MUST NOT survive this transition.
                self.owner.mode_ticket = None;
                Ok(())
            }
        }
    }
    fn receipt(&self, result: Result<(), Refusal>) -> Receipt {
        let outcome = if self.uncertain {
            InputOutcome::EffectUnknown
        } else if result.is_ok() {
            if self.submitted == 0 {
                InputOutcome::AppliedLocally
            } else {
                InputOutcome::SubmittedToOs
            }
        } else if self.submitted > 0 {
            InputOutcome::PartiallySubmittedToOs
        } else {
            match result {
                Err(Refusal::Authority(
                    AuthorityError::TicketExpired
                    | AuthorityError::LeaseExpired
                    | AuthorityError::ObservationExpired,
                )) => InputOutcome::ExpiredBeforeSubmission,
                Err(Refusal::Revoked) => InputOutcome::CancelledBeforeSubmission,
                _ => InputOutcome::RejectedBeforeSubmission,
            }
        };
        Receipt {
            sequence: self.request.sequence,
            outcome,
            submitted_operations: self.submitted,
            refusal: result.err(),
        }
    }
    fn finish(mut self, result: Result<(), Refusal>) -> Receipt {
        let receipt = self.receipt(result);
        self.owner.record(receipt, self.pointer);
        self.completed = true;
        receipt
    }
}
impl Drop for Attempt<'_, '_> {
    fn drop(&mut self) {
        if !self.completed {
            let receipt = self.receipt(Err(if self.uncertain {
                Refusal::UnknownEffect
            } else {
                Refusal::Revoked
            }));
            self.owner.record(receipt, self.pointer);
        }
    }
}
