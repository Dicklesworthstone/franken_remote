//! Bounded OS-event handoff to the original controlled session. The UI thread
//! never borrows QUIC or holds an authority lock while making native calls.
mod native;
use super::{ControlledViewer, ViewerControl, now};
use fr_client::input::{self, Action};
pub use fr_client::input::{
    ClientInstant,
    viewport::{Layout, LocalPoint, Located, PositionedAction},
};
pub use fr_core::held_state::HeldState;
pub use fr_core::input::{CommittedText, TextError};
use fr_core::{
    input::{KeyTransition, MAX_COMMITTED_TEXT_BYTES, PhysicalKey, PointerButton},
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
    Overflow,
    Unavailable,
    UnsupportedText,
    Text(TextError),
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
    pending: Option<Captured>,
    /// Presses dropped as aged: their later release or repeat goes with them.
    dropped_keys: [bool; 256],
    dropped_buttons: [bool; 5],
    /// Captured events dropped as obsolete local input (counts only).
    dropped: u64,
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
    /// The host-granted capability, not permission to bypass freshness or expiry.
    pub const fn supports_text(&self) -> bool {
        self.capabilities.contains(Capability::Text)
    }
    /// Capture one completed IME/software-keyboard commit. Do not also send the
    /// physical keys that produced the same text. No clipboard or layout fallback.
    /// Oversized commits are refused whole, never split into separately retried actions.
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
        // Capability refusal admitted nothing. Keep physical-key operation usable.
        if result.is_err() && result != Err(Error::UnsupportedText) {
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
    fn push_inner(&mut self, event: Event, sampled: ClientInstant) -> Result<(), Error> {
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
        if sampled < queue.last_sample {
            return Err(Error::Clock);
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
fn not_future(sampled: ClientInstant, current: ClientInstant) -> Result<(), Error> {
    current
        .0
        .checked_sub(sampled.0)
        .ok_or(Error::Clock)
        .map(|_| ())
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
    fn new(queue: Arc<Mutex<Queue>>) -> Self {
        Self {
            queue,
            pending: None,
            dropped_keys: [false; 256],
            dropped_buttons: [false; 5],
            dropped: 0,
        }
    }
    pub(super) const fn dropped(&self) -> u64 {
        self.dropped
    }
    /// Local input older than its dispatch bound is obsolete work (AGENTS.md
    /// 5), never a reason to end control: an aged press, repeat, motion,
    /// scroll or text is dropped and counted, never sent late. Cleanup
    /// (`cleanup`) stays queued and is sent late. Front only, so nothing is
    /// reordered. A view that awaits its visibility callback holds dispatch,
    /// so events can age even while the loop runs (fr-1r40).
    pub(super) fn expire(
        &mut self,
        current: ClientInstant,
        holds: &impl Fn(Held) -> bool,
    ) -> Result<(), Error> {
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
        match event {
            Event::Key {
                key,
                transition: KeyTransition::Press,
            } => self.dropped_keys[usize::from(key.usage())] = true,
            Event::Positioned {
                action:
                    PositionedAction::Button {
                        button,
                        pressed: true,
                    },
                ..
            } => self.dropped_buttons[*button as usize - 1] = true,
            _ => {}
        }
    }
    fn front(&mut self) -> Result<Option<&Captured>, Error> {
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
        self.events = Some(Receiver::new(queue.clone()));
        Ok(Source {
            queue: Arc::downgrade(&queue),
            control: self.control(),
            capabilities: self.input.capabilities(),
        })
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
    fn dispatch_one(&mut self, receiver: &mut Receiver) -> Result<(), super::Error> {
        let at = self.check_inner()?;
        let input = &self.input;
        receiver
            .expire(at, &|held| match held {
                Held::Key(key) => input.holds_key(key),
                Held::Button(button) => input.holds_button(button),
            })
            .map_err(super::Error::Capture)?;
        if receiver.front().map_err(super::Error::Capture)?.is_none() || self.pending_send() {
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
                receiver.pending = None;
                Ok(())
            }
            Err(
                super::Error::Backpressure
                | super::Error::View(input::presentation::Error::Input(input::Error::Backpressure)),
            ) => Ok(()),
            // A stale view suspends input (plan 11.3): this event is refused and
            // dropped, never queued for later. So is a release of a key or button
            // that suspension already released on the host. The session continues.
            Err(super::Error::View(input::presentation::Error::Input(
                input::Error::ViewSuspended | input::Error::ReleasedBySuspension,
            ))) => {
                receiver.pending = None;
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
