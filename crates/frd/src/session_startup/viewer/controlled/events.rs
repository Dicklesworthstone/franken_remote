//! Bounded OS-event handoff to the original controlled session. The UI thread
//! never borrows QUIC or holds an authority lock while making native calls.
mod native;
#[cfg(test)]
mod native_deadline_tests;
#[cfg(test)]
mod text_context_tests;
#[cfg(test)]
mod text_view_tests;
use super::{ControlledViewer, ViewerControl, now};
use fr_client::input::{self, Action};
pub use fr_client::input::{
    ClientInstant,
    viewport::{Layout, LocalPoint, Located, PositionedAction},
};
pub use fr_core::held_state::HeldState;
pub use fr_core::input::{CommittedText, TextError};
use fr_core::{
    input::{InputView, KeyTransition, MAX_COMMITTED_TEXT_BYTES, PhysicalKey, PointerButton},
    input_submission::{Capabilities, Capability},
};
pub use native::{CaptureCleanup, CaptureReapError, CaptureStartError, NativeCapture};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, TryLockError, Weak},
};

pub const MAX_EVENTS: usize = 64;
pub const MAX_EVENT_AGE_US: u64 = 100_000;
/// Heap payload bound across the queue and its one deferred dispatch event.
/// Fixed event metadata and the existing encoded send slot are bounded separately.
pub const MAX_RETAINED_TEXT_BYTES: usize = (MAX_EVENTS + 1) * MAX_COMMITTED_TEXT_BYTES;

/// No Debug: physical keys, pointer coordinates and text are not diagnostics.
pub enum Event {
    Key {
        key: PhysicalKey,
        transition: KeyTransition,
    },
    /// A whole committed Unicode value, never an IME preedit update.
    Text(CommittedText),
    /// Actual local platform state, used only to reconcile already-sent presses.
    HeldState(HeldState),
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
    UnsupportedText,
    StaleTextContext,
    TextNotReady,
    Text(TextError),
}
/// A local draft belongs to one original producer and one uninterrupted input
/// context. It contains no ticket and grants no authority. Capture it BEFORE
/// entering native text handling; never substitute a later token for a draft
/// or commit that the platform already produced.
#[derive(Clone)]
pub struct TextContext {
    queue: Weak<Mutex<Queue>>,
    state: TextState,
}
impl PartialEq for TextContext {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.queue, &other.queue) && self.state == other.state
    }
}
impl Eq for TextContext {}
impl TextContext {
    fn belongs_to(&self, queue: &Weak<Mutex<Queue>>) -> bool {
        Weak::ptr_eq(&self.queue, queue)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextState {
    view: InputView,
    revision: u64,
    suspensions: u32,
    suspended: bool,
}
impl TextState {
    fn new(view: InputView, suspensions: u32, suspended: bool) -> Result<Self, Error> {
        // The input owner's diagnostic count saturates. Refuse before that
        // history could hide a later lapse instead of treating equal counts
        // as proof of uninterrupted readiness.
        if suspensions == u32::MAX {
            return Err(Error::Unavailable);
        }
        Ok(Self {
            view,
            revision: 0,
            suspensions,
            suspended,
        })
    }
    fn update(&mut self, view: InputView, suspensions: u32, suspended: bool) -> Result<(), Error> {
        if suspensions < self.suspensions || suspensions == u32::MAX {
            return Err(Error::Unavailable);
        }
        if self.view != view || self.suspensions != suspensions || self.suspended != suspended {
            self.revision = self.revision.checked_add(1).ok_or(Error::Unavailable)?;
            self.view = view;
            self.suspensions = suspensions;
            self.suspended = suspended;
        }
        Ok(())
    }
}
struct Captured {
    event: Event,
    sampled: ClientInstant,
    /// A Unicode commit cannot acquire a replacement view while queued.
    /// Physical positioned events retain their existing checked Layout.
    text_context: Option<TextState>,
    /// An original native send bound, never a renewed ticket. Releases remain
    /// eligible for cleanup after this bound if the host already holds them.
    send_until: Option<ClientInstant>,
}
struct Queue {
    events: VecDeque<Captured>,
    last_sample: ClientInstant,
    /// Last view checked by this original viewer's dispatch owner. A producer
    /// racing a replacement sees the old value and is conservatively refused.
    context: TextState,
    /// A compositor candidate can briefly make Send unavailable without
    /// cancelling a valid local draft or changing its original context.
    text_ready: bool,
}
impl Queue {
    fn context_ready(&self, context: TextState) -> Result<bool, Error> {
        if context.suspended || context != self.context {
            return Err(Error::StaleTextContext);
        }
        Ok(self.text_ready)
    }
    fn push(
        &mut self,
        event: Event,
        sampled: ClientInstant,
        context: Option<TextState>,
    ) -> Result<(), Error> {
        self.push_bounded(event, sampled, context, None)
    }
    fn push_bounded(
        &mut self,
        event: Event,
        sampled: ClientInstant,
        context: Option<TextState>,
        until: Option<ClientInstant>,
    ) -> Result<(), Error> {
        if let Some(context) = context
            && !self.context_ready(context)?
        {
            return Err(Error::TextNotReady);
        }
        if sampled < self.last_sample {
            return Err(Error::Clock);
        }
        self.last_sample = sampled;
        // Only adjacent pointer moves coalesce, never across an action.
        if matches!(event, Event::Pointer(_))
            && self
                .events
                .back()
                .is_some_and(|e| matches!(e.event, Event::Pointer(_)))
        {
            self.events.pop_back();
        }
        if self.events.len() == MAX_EVENTS {
            return Err(Error::Overflow);
        }
        let text_context = matches!(event, Event::Text(_)).then_some(self.context);
        self.events.push_back(Captured {
            event,
            sampled,
            text_context,
            send_until: until,
        });
        Ok(())
    }
}
/// A single native event producer. Drop, focus loss, hiding, resize, suspend and
/// native failure must stop it. The authority fence is lock-free and wakes
/// cancellation of an in-flight network operation. Best-effort queue cleanup
/// never waits for a lock; no subsequent focus gain can resume control.
/// Capture timestamps must use `clock`, not wall time or the host's clock.
/// A retained stopped producer cannot keep the closed viewer's payloads alive.
pub struct Source {
    queue: Weak<Mutex<Queue>>,
    control: ViewerControl,
    capabilities: Capabilities,
}
pub(super) struct Receiver {
    queue: Arc<Mutex<Queue>>,
    /// Authoritative even when the producer currently holds the queue. A
    /// deferred publication must not restore a context after a suspension.
    context: TextState,
    text_ready: bool,
    pending: Option<Captured>,
    /// Presses dropped as aged: their later release or repeat goes with them.
    dropped_keys: [bool; 256],
    dropped_buttons: [bool; 5],
    /// Captured events dropped as obsolete local input (counts only).
    dropped: u64,
    /// Presses a stale view refused (plan 11.3). The host never saw them, so
    /// a release or repeat after input resumes goes with them too.
    refused_keys: [bool; 256],
    refused_buttons: [bool; 5],
    /// Captured events dropped because a stale view suspended input,
    /// including those followers (counts only).
    suspended: u64,
}
/// What the host may hold for this grant: a sent press not yet released.
#[derive(Debug, Clone, Copy)]
pub(super) enum Held {
    Key(PhysicalKey),
    Button(PointerButton),
}
impl Source {
    /// The ORIGINAL viewer's terminal fence, for a native supervisor that must
    /// stop independently of a blocked platform call. This never grants input.
    pub fn control(&self) -> ViewerControl {
        self.control.clone()
    }
    /// Positively granted operations. Native adapters must not emulate missing
    /// text, repeat or scroll modes with a different input operation.
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

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
        if let Some(queue) = self.queue.upgrade() {
            match queue.try_lock() {
                Ok(mut queue) => queue.events.clear(),
                Err(TryLockError::Poisoned(error)) => error.into_inner().events.clear(),
                Err(TryLockError::WouldBlock) => {}
            }
        }
    }
    /// Captured events not yet taken for dispatch, so a native producer can
    /// pace a burst (pasted or dictated text typed as keys) below
    /// [`MAX_EVENTS`] instead of overflowing, which is terminal. `None` when
    /// it cannot tell without waiting: the dispatcher holds the queue, or the
    /// viewer is gone. Never a grant, and never waits for a lock.
    pub fn queued(&self) -> Option<usize> {
        let queue = self.queue.upgrade()?;
        match queue.try_lock() {
            Ok(queue) => Some(queue.events.len()),
            Err(TryLockError::Poisoned(_) | TryLockError::WouldBlock) => None,
        }
    }
    /// The host-granted capability, not permission to bypass freshness or expiry.
    pub const fn supports_text(&self) -> bool {
        self.capabilities.contains(Capability::Text)
    }
    /// Content-free context for a new local composition. `None` means input
    /// is suspended. Ordinary compositor candidate gaps preserve the context;
    /// `text_context_ready` separately reports the last checked Send eligibility.
    /// Equality includes source identity, every view generation and suspension
    /// history, even when the same view resumes before the next publication.
    pub fn text_context(&self) -> Result<Option<TextContext>, Error> {
        self.clock()?;
        if !self.supports_text() {
            return Err(Error::UnsupportedText);
        }
        let shared = self.queue.upgrade().ok_or(Error::Closed)?;
        let queue = shared.lock().map_err(|_| Error::Unavailable)?;
        if self.control.is_stopped() {
            return Err(Error::Closed);
        }
        Ok((!queue.context.suspended).then(|| TextContext {
            queue: self.queue.clone(),
            state: queue.context,
        }))
    }
    /// Last owner-checked eligibility for this exact draft. This is local UI
    /// state, not permission: dispatch still checks the original view, ticket
    /// and authority immediately before encoding and transport admission.
    pub fn text_context_ready(&self, context: &TextContext) -> Result<bool, Error> {
        self.clock()?;
        if !self.supports_text() {
            return Err(Error::UnsupportedText);
        }
        if !context.belongs_to(&self.queue) {
            return Err(Error::StaleTextContext);
        }
        let shared = self.queue.upgrade().ok_or(Error::Closed)?;
        let queue = shared.lock().map_err(|_| Error::Unavailable)?;
        if self.control.is_stopped() {
            return Err(Error::Closed);
        }
        queue.context_ready(context.state)
    }
    /// Submit a native commit under the context that began its local draft.
    /// Stale/replaced context is a whole-value refusal with no wire identity
    /// or retry; physical-key operation remains available on the same source.
    pub fn commit_text_in_context(
        &mut self,
        context: &TextContext,
        text: &str,
        sampled: ClientInstant,
    ) -> Result<(), Error> {
        self.capture_text_in_context(context, text, sampled, None)
    }
    /// Native completion additionally freezes the original checked client-clock
    /// send deadline. Neither queueing nor later same-context ticket renewal can
    /// extend it. A stale deadline refuses the whole commit without stopping keys.
    pub fn commit_text_in_context_until(
        &mut self,
        context: &TextContext,
        text: &str,
        sampled: ClientInstant,
        until: ClientInstant,
    ) -> Result<(), Error> {
        self.capture_text_in_context(context, text, sampled, Some(until))
    }
    fn capture_text_in_context(
        &mut self,
        context: &TextContext,
        text: &str,
        sampled: ClientInstant,
        until: Option<ClientInstant>,
    ) -> Result<(), Error> {
        let result = (|| {
            let current = self.clock()?;
            not_future(sampled, current)?;
            if until.is_some_and(|until| current >= until) {
                return Err(Error::TextNotReady);
            }
            if !self.supports_text() {
                return Err(Error::UnsupportedText);
            }
            if !context.belongs_to(&self.queue) {
                return Err(Error::StaleTextContext);
            }
            let text = CommittedText::new(text).map_err(Error::Text)?;
            self.push_bounded(Event::Text(text), sampled, Some(context.state), until)
        })();
        self.finish_capture(result)
    }
    /// Capture one completed IME/software-keyboard commit. Do not also send the
    /// physical keys that produced the same text. No clipboard or layout fallback.
    /// Oversized commits are refused whole, never split into separately retried actions.
    /// The original dispatch view is pinned with the commit; a queued value
    /// cannot be rebound to new geometry, viewport, configuration or recovery.
    /// A native draft edited across callbacks must instead retain `TextContext`
    /// from its beginning and use `commit_text_in_context` on explicit Send.
    pub fn commit_text(&mut self, text: &str, sampled: ClientInstant) -> Result<(), Error> {
        let result = (|| {
            not_future(sampled, self.clock()?)?;
            if !self.capabilities.contains(Capability::Text) {
                return Err(Error::UnsupportedText);
            }
            let text = CommittedText::new(text).map_err(Error::Text)?;
            self.push_inner(Event::Text(text), sampled)
        })();
        self.finish_capture(result)
    }
    /// Capture the platform's actual key/button snapshot in the same ordered
    /// queue as input. Missing state releases remotely held input; extra held
    /// bits never synthesize presses. Snapshots are not coalesced across actions.
    /// Sample periodically: the existing sender permits at most four snapshots
    /// per second and discards early samples without postponing or retiming them.
    /// Focus loss, hiding and suspension still require immediate `stop` instead.
    pub fn reconcile_held(
        &mut self,
        observed: HeldState,
        sampled: ClientInstant,
    ) -> Result<(), Error> {
        self.push(Event::HeldState(observed), sampled)
    }
    fn finish_capture(&self, result: Result<(), Error>) -> Result<(), Error> {
        // These whole-value refusals admitted nothing. Keep physical keys
        // usable, and let a still-valid local draft await a compositor gap.
        if result.is_err()
            && !matches!(
                result,
                Err(Error::UnsupportedText | Error::StaleTextContext | Error::TextNotReady)
            )
        {
            self.stop();
        }
        result
    }
    /// Submit an event stamped when sampled, never when dequeued after a stall.
    /// Adjacent motion replaces motion, but never crosses an action barrier.
    /// Except for unsupported text refused before admission, failure is terminal:
    /// a dropped release cannot leave a live remote drag.
    pub fn push(&mut self, event: Event, sampled: ClientInstant) -> Result<(), Error> {
        let result = self.push_inner(event, sampled);
        self.finish_capture(result)
    }
    /// Capture an event with its already established exclusive send deadline.
    /// Native queues may consume part of the freshness budget before calling
    /// this method. Neither this source nor transport backpressure renews it.
    /// A refused capture is terminal, so a lost release triggers held cleanup.
    pub fn push_until(
        &mut self,
        event: Event,
        sampled: ClientInstant,
        until: ClientInstant,
    ) -> Result<(), Error> {
        let result = (|| {
            let current = self.clock()?;
            not_future(sampled, current)?;
            if until <= current {
                return Err(Error::Expired);
            }
            let latest = sampled
                .0
                .checked_add(MAX_EVENT_AGE_US)
                .ok_or(Error::Clock)?;
            if until.0 > latest {
                return Err(Error::Clock);
            }
            self.push_bounded(event, sampled, None, Some(until))
        })();
        self.finish_capture(result)
    }
    fn push_inner(&mut self, event: Event, sampled: ClientInstant) -> Result<(), Error> {
        self.push_in_context(event, sampled, None)
    }
    fn push_in_context(
        &mut self,
        event: Event,
        sampled: ClientInstant,
        context: Option<TextState>,
    ) -> Result<(), Error> {
        self.push_bounded(event, sampled, context, None)
    }
    fn push_bounded(
        &mut self,
        event: Event,
        sampled: ClientInstant,
        context: Option<TextState>,
        until: Option<ClientInstant>,
    ) -> Result<(), Error> {
        let current = self.clock()?;
        // An aged event is still admitted: the session drops it as obsolete,
        // or sends it late if it releases what the host holds (`expire`).
        not_future(sampled, current)?;
        if matches!(event, Event::Text(_)) && !self.capabilities.contains(Capability::Text) {
            return Err(Error::UnsupportedText);
        }
        let shared = self.queue.upgrade().ok_or(Error::Closed)?;
        let mut queue = shared.lock().map_err(|_| Error::Unavailable)?;
        if self.control.is_stopped() {
            return Err(Error::Closed);
        }
        if let Some(until) = until {
            queue.push_bounded(event, sampled, context, Some(until))
        } else {
            queue.push(event, sampled, context)
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.stop();
    }
}
fn not_future(sampled: ClientInstant, current: ClientInstant) -> Result<(), Error> {
    current
        .0
        .checked_sub(sampled.0)
        .ok_or(Error::Clock)
        .map(|_| ())
}
fn native_deadline_expired(captured: &Captured, current: ClientInstant) -> bool {
    !matches!(captured.event, Event::Text(_))
        && captured.send_until.is_some_and(|until| current >= until)
}
fn aged(sampled: ClientInstant, current: ClientInstant) -> Result<bool, Error> {
    let age = current.0.checked_sub(sampled.0).ok_or(Error::Clock)?;
    Ok(age >= MAX_EVENT_AGE_US)
}
/// A release of what the host holds, or held-state reconciliation, is cleanup
/// (AGENTS.md 4: releasing is cleanup, not rollback): delivered late rather
/// than left held on the host.
fn cleanup(event: &Event, holds: &impl Fn(Held) -> bool) -> bool {
    match event {
        Event::Key {
            key,
            transition: KeyTransition::Release,
        } => holds(Held::Key(*key)),
        Event::Positioned {
            action:
                PositionedAction::Button {
                    button,
                    pressed: false,
                },
            ..
        } => holds(Held::Button(*button)),
        Event::HeldState(_) => true,
        _ => false,
    }
}
/// The repeat or release of a press dropped as aged (a release clears the mark).
fn follows_dropped(event: &Event, keys: &mut [bool; 256], buttons: &mut [bool; 5]) -> bool {
    match event {
        Event::Key { key, transition } => {
            let slot = &mut keys[usize::from(key.usage())];
            let follows = *slot && *transition != KeyTransition::Press;
            if follows && *transition == KeyTransition::Release {
                *slot = false;
            }
            follows
        }
        Event::Positioned {
            action: PositionedAction::Button { button, pressed },
            ..
        } => {
            let slot = &mut buttons[*button as usize - 1];
            let follows = *slot && !*pressed;
            if follows {
                *slot = false;
            }
            follows
        }
        _ => false,
    }
}
/// Record a press the host never saw, so its later release or repeat goes
/// with it.
fn unseen_press(event: &Event, keys: &mut [bool; 256], buttons: &mut [bool; 5]) {
    match event {
        Event::Key {
            key,
            transition: KeyTransition::Press,
        } => keys[usize::from(key.usage())] = true,
        Event::Positioned {
            action:
                PositionedAction::Button {
                    button,
                    pressed: true,
                },
            ..
        } => buttons[*button as usize - 1] = true,
        _ => {}
    }
}
/// A press that reaches the session supersedes an earlier dropped one.
fn sent(event: &Event, keys: &mut [bool; 256], buttons: &mut [bool; 5]) {
    match event {
        Event::Key {
            key,
            transition: KeyTransition::Press,
        } => keys[usize::from(key.usage())] = false,
        Event::Positioned {
            action:
                PositionedAction::Button {
                    button,
                    pressed: true,
                },
            ..
        } => buttons[*button as usize - 1] = false,
        _ => {}
    }
}
impl Receiver {
    fn new(queue: Arc<Mutex<Queue>>, context: TextState, text_ready: bool) -> Self {
        Self {
            queue,
            context,
            text_ready,
            pending: None,
            dropped_keys: [false; 256],
            dropped_buttons: [false; 5],
            dropped: 0,
            refused_keys: [false; 256],
            refused_buttons: [false; 5],
            suspended: 0,
        }
    }
    pub(super) const fn dropped(&self) -> u64 {
        self.dropped
    }
    pub(super) const fn suspended(&self) -> u64 {
        self.suspended
    }
    /// Ordinary local input older than its sample-age bound is obsolete work
    /// (AGENTS.md 5): an aged press, repeat, motion, scroll or text is dropped
    /// and counted, never sent late. An explicit native send deadline also
    /// protects ordered dependent keys; losing that event is terminal below.
    /// Cleanup (`cleanup`) stays queued and is sent late. Front only, so
    /// nothing is reordered. A view that awaits its visibility callback holds
    /// dispatch, so events can age even while the loop runs (fr-1r40).
    pub(super) fn expire(
        &mut self,
        current: ClientInstant,
        holds: &impl Fn(Held) -> bool,
    ) -> Result<(), Error> {
        // A native producer that lost a bounded key cannot continue with
        // dependent keys under a different modifier state. Text retains its
        // independent whole-commit refusal; held releases remain cleanup.
        if let Some(pending) = &self.pending
            && native_deadline_expired(pending, current)
            && !cleanup(&pending.event, holds)
        {
            return Err(Error::Expired);
        }
        if let Some(pending) = &self.pending
            && aged(pending.sampled, current)?
            && !cleanup(&pending.event, holds)
        {
            let Captured { event, .. } = self.pending.take().ok_or(Error::Unavailable)?;
            self.note_dropped(&event);
        }
        let queue = self.queue.clone();
        match queue.try_lock() {
            Ok(mut queue) => {
                while let Some(first) = queue.events.front() {
                    if native_deadline_expired(first, current) && !cleanup(&first.event, holds) {
                        return Err(Error::Expired);
                    }
                    if !aged(first.sampled, current)? || cleanup(&first.event, holds) {
                        break;
                    }
                    let Captured { event, .. } =
                        queue.events.pop_front().ok_or(Error::Unavailable)?;
                    self.note_dropped(&event);
                }
                Ok(())
            }
            Err(TryLockError::WouldBlock) => Ok(()),
            Err(TryLockError::Poisoned(_)) => Err(Error::Unavailable),
        }
    }
    fn note_dropped(&mut self, event: &Event) {
        self.dropped = self.dropped.saturating_add(1);
        unseen_press(event, &mut self.dropped_keys, &mut self.dropped_buttons);
    }
    fn note_refused(&mut self, event: &Event) {
        self.suspended = self.suspended.saturating_add(1);
        unseen_press(event, &mut self.refused_keys, &mut self.refused_buttons);
    }
    fn front(&mut self, view: InputView, now: ClientInstant) -> Result<Option<&Captured>, Error> {
        self.context
            .update(view, self.context.suspensions, self.context.suspended)?;
        // Publish only the dispatch owner's checked view; native code cannot
        // update it, and this never changes an already captured commit.
        match self.queue.try_lock() {
            Ok(mut queue) => {
                queue.context = self.context;
                queue.text_ready = self.text_ready;
                if self.pending.is_none() {
                    self.pending = queue.events.pop_front();
                }
            }
            Err(TryLockError::WouldBlock) if self.pending.is_none() => return Ok(None),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Poisoned(_)) => return Err(Error::Unavailable),
        }
        if self.pending.as_ref().is_some_and(|captured| {
            captured
                .text_context
                .is_some_and(|original| original.suspended || original != self.context)
                || (matches!(captured.event, Event::Text(_))
                    && captured.send_until.is_some_and(|until| now >= until))
        }) {
            let Captured { event, .. } = self.pending.take().ok_or(Error::Unavailable)?;
            self.note_refused(&event);
            return Ok(None);
        }
        Ok(self.pending.as_ref())
    }
    fn publish_context(
        &mut self,
        view: InputView,
        suspensions: u32,
        suspended: bool,
        ready: bool,
    ) -> Result<(), Error> {
        self.context.update(view, suspensions, suspended)?;
        self.text_ready = ready && !suspended;
        match self.queue.try_lock() {
            Ok(mut queue) => {
                queue.context = self.context;
                queue.text_ready = self.text_ready;
                Ok(())
            }
            Err(TryLockError::WouldBlock) => Ok(()),
            Err(TryLockError::Poisoned(_)) => Err(Error::Unavailable),
        }
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
        let ready = self.input.tick(at).map_err(super::Error::View)?;
        let context = TextState::new(
            self.input.input_view(),
            self.input.suspension_totals(at).0,
            self.input.suspended_since().is_some(),
        )
        .map_err(super::Error::Capture)?;
        let queue = Arc::new(Mutex::new(Queue {
            events,
            last_sample: at,
            context,
            text_ready: ready,
        }));
        self.events = Some(Receiver::new(queue.clone(), context, ready));
        Ok(Source {
            queue: Arc::downgrade(&queue),
            control: self.control(),
            capabilities: self.input.capabilities(),
        })
    }
    pub(super) fn publish_text_context(
        &mut self,
        ready: bool,
        at: ClientInstant,
    ) -> Result<(), super::Error> {
        let view = self.input.input_view();
        let suspensions = self.input.suspension_totals(at).0;
        let suspended = self.input.suspended_since().is_some();
        if let Some(receiver) = &mut self.events {
            receiver
                .publish_context(view, suspensions, suspended, ready)
                .map_err(super::Error::Capture)?;
        }
        Ok(())
    }
    pub(super) fn dispatch_captured(&mut self) -> Result<(), super::Error> {
        let Some(mut receiver) = self.events.take() else {
            return Ok(());
        };
        let result = self.dispatch_one(&mut receiver);
        if !self.is_closed() {
            self.events = Some(receiver);
        }
        result
    }
    #[allow(clippy::too_many_lines)] // One captured event's ordered fences: view, age, suspension, send.
    fn dispatch_one(&mut self, receiver: &mut Receiver) -> Result<(), super::Error> {
        let at = self.check_inner()?;
        // The receiver is temporarily outside `self.events`: publish directly
        // after the maintenance check, including a lapse it discovered while
        // the producer held the queue or the same view has already resumed.
        let ready = self.input.tick(at).map_err(super::Error::View)?;
        receiver
            .publish_context(
                self.input.input_view(),
                self.input.suspension_totals(at).0,
                self.input.suspended_since().is_some(),
                ready,
            )
            .map_err(super::Error::Capture)?;
        let input = &self.input;
        receiver
            .expire(at, &|held| match held {
                Held::Key(key) => input.holds_key(key),
                Held::Button(button) => input.holds_button(button),
            })
            .map_err(super::Error::Capture)?;
        if receiver
            .front(self.input.input_view(), at)
            .map_err(super::Error::Capture)?
            .is_none()
            || self.pending_send()
            || (!ready && self.input.suspended_since().is_none())
        {
            return Ok(());
        }
        let Some(captured) = receiver.pending.as_ref() else {
            return Ok(());
        };
        // The repeat or release of a press dropped as aged: the host never saw
        // the press, so this goes with it.
        if follows_dropped(
            &captured.event,
            &mut receiver.dropped_keys,
            &mut receiver.dropped_buttons,
        ) {
            receiver.dropped = receiver.dropped.saturating_add(1);
            receiver.pending = None;
            return Ok(());
        }
        // Likewise for a press a stale view refused, even once input resumed.
        if follows_dropped(
            &captured.event,
            &mut receiver.refused_keys,
            &mut receiver.refused_buttons,
        ) {
            receiver.suspended = receiver.suspended.saturating_add(1);
            receiver.pending = None;
            return Ok(());
        }
        let result = match &captured.event {
            Event::Key { key, transition } => self
                .action(Action::Key {
                    key: *key,
                    transition: *transition,
                })
                .map(|_| ()),
            Event::Text(text) => self.action(Action::Text(text.as_str())).map(|_| ()),
            Event::HeldState(observed) => self.reconcile_held(*observed).map(|_| ()),
            Event::Pointer(location) => self.pointer_in_view(location).map(|_| ()),
            Event::Positioned { location, action } => {
                self.action_in_view(location, *action).map(|_| ())
            }
        };
        match result {
            Ok(()) => {
                // Native event age survives encoding AND transport backpressure.
                // Cleanup sent after its age bound keeps the send deadline it
                // was given: clamping it to the past would expire it at once.
                let deadline = captured
                    .sampled
                    .0
                    .checked_add(MAX_EVENT_AGE_US)
                    .ok_or(super::Error::Capture(Error::Clock))?;
                let deadline = captured
                    .send_until
                    .map_or(deadline, |until| deadline.min(until.0));
                if deadline > at.0
                    && let Some(pending) = &mut self.pending
                {
                    pending.until = pending.until.min(deadline);
                }
                sent(
                    &captured.event,
                    &mut receiver.dropped_keys,
                    &mut receiver.dropped_buttons,
                );
                sent(
                    &captured.event,
                    &mut receiver.refused_keys,
                    &mut receiver.refused_buttons,
                );
                receiver.pending = None;
                Ok(())
            }
            Err(
                super::Error::Backpressure
                | super::Error::View(input::presentation::Error::Input(input::Error::Backpressure)),
            ) => Ok(()),
            // A stale view suspends input (plan 11.3): this event is refused and
            // dropped, never queued for later. So is a release of a key or button
            // that suspension already released on the host. A refused press is
            // remembered: its release may come after input resumes, and the host
            // never saw the press. The session continues.
            Err(super::Error::View(input::presentation::Error::Input(
                input::Error::ViewSuspended | input::Error::ReleasedBySuspension,
            ))) => {
                let Captured { event, .. } = receiver
                    .pending
                    .take()
                    .ok_or(super::Error::Capture(Error::Unavailable))?;
                receiver.note_refused(&event);
                Ok(())
            }
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
