//! Software-keyboard callbacks retain their original source, view and deadline
//! while paced by the UI. All physical-key sequencing stays in Rust.
#![forbid(unsafe_code)]

use super::{ClientInstant, Granted, InputResult, KeyTransition, Shared, input, key_capture};
use fr_core::input_submission::Capability;
use frd::session_startup::viewer_events::{InputContext, MAX_EVENT_AGE_US};
use input::typing::{Batch, Plan, Stroke};

pub(super) struct Capture {
    context: InputContext,
    time: key_capture::Capture,
}

impl Shared {
    /// Capture once at the start of the original software-keyboard callback,
    /// before converting its text or waiting for input capacity. A refusal
    /// admits nothing; the caller must discard that completed callback.
    pub fn begin_typing(&self) -> Result<u64, InputResult> {
        let started = std::time::Instant::now();
        let mut token = None;
        let result = self.with_granted(|g| {
            if !g.source.capabilities().contains(Capability::Keys) {
                return InputResult::Unsupported;
            }
            let Ok(now) = g.source.clock() else {
                return InputResult::ControlLost;
            };
            let Some((context, until)) = &g.key_window else {
                return InputResult::Busy;
            };
            match g.source.input_context_ready(context) {
                Ok(true) => {}
                Ok(false) | Err(_) => return InputResult::Busy,
            }
            let Some(age) = until.0.checked_sub(now.0).filter(|age| *age > 0) else {
                return InputResult::Busy;
            };
            let Some(time) =
                key_capture::Capture::new(&g.stop, g.opened.0, now.0, age.min(MAX_EVENT_AGE_US))
                    .and_then(|capture| capture.with_queue_age(started.elapsed()))
            else {
                // Nothing entered the ordered source. A callback that waited
                // across grant attachment may not end the successor's control.
                return InputResult::Busy;
            };
            let capture = Capture {
                context: context.clone(),
                time,
            };
            token = g.typing.insert(capture);
            if token.is_some() {
                InputResult::Pending
            } else {
                InputResult::Busy
            }
        });
        token.ok_or(result)
    }

    /// One complete physical stroke under its original capture and index.
    /// Busy means zero transitions were queued, so only this same stroke may
    /// retry. Accepted indices are spent. Expiry or context loss fences the
    /// original source before dependent input, allowing held-state cleanup.
    pub fn type_captured(&self, token: u64, index: u32, usage: u16, modifiers: u8) -> InputResult {
        if input::key(usage).is_none() {
            return InputResult::Invalid;
        }
        let stroke = Stroke { usage, modifiers };
        self.with_granted(|g| {
            let Some(mut batch) = g.typing.take(token) else {
                // A token from a replaced source cannot stop or type into its
                // successor. Every issued token is globally non-repeating.
                return InputResult::Invalid;
            };
            let result = type_stroke(g, &mut batch, index, stroke);
            if matches!(
                result,
                InputResult::Sent | InputResult::Pending | InputResult::Busy
            ) {
                g.typing.restore(batch);
            }
            result
        })
    }

    /// Retire a completed/cancelled callback. Private metadata only; already
    /// admitted strokes and their host effects are never replayed or undone.
    pub fn end_typing(&self, token: u64) {
        let _ = self.with_granted(|g| {
            drop(g.typing.take(token));
            InputResult::Pending
        });
    }
}

fn type_stroke(
    g: &mut Granted,
    batch: &mut Batch<Capture>,
    index: u32,
    stroke: Stroke,
) -> InputResult {
    if !batch.begin(index, stroke) {
        return InputResult::ControlLost;
    }
    let Ok(now) = g.source.clock() else {
        return InputResult::ControlLost;
    };
    if !batch.capture.time.current(&g.stop, now.0) {
        return InputResult::ControlLost;
    }
    match g.source.input_context_ready(&batch.capture.context) {
        Ok(true) => {}
        Ok(false) => return InputResult::Busy,
        Err(_) => return InputResult::ControlLost,
    }
    let plan = Plan::new(stroke, &g.keys);
    let events = plan.events();
    if events.is_empty() {
        batch.accepted();
        return InputResult::Pending;
    }
    // Source has exactly one producer under this Granted lock. Dispatch can
    // only shrink its queue while this entire stroke is admitted. Unknown
    // queue occupancy is backpressure, never assumed spare capacity.
    if g.source.queued().is_none_or(|queued| {
        queued.saturating_add(events.len()) > super::PACED_EVENTS.max(events.len())
    }) {
        return InputResult::Busy;
    }
    g.scroll.reset();
    let (sampled, until) = batch.capture.time.bounds();
    for event in events {
        let Some((slot, key)) = input::key(event.usage) else {
            return InputResult::ControlLost;
        };
        let transition = if event.pressed {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        };
        if g.source
            .push_key_in_context_until(
                &batch.capture.context,
                key,
                transition,
                ClientInstant(sampled),
                ClientInstant(until),
            )
            .is_err()
        {
            return InputResult::ControlLost;
        }
        g.keys[slot] = event.pressed;
    }
    batch.accepted();
    InputResult::Sent
}
