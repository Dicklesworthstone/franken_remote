//! A native editor belongs to its original input source and uninterrupted view.
//! Editing is local state; only an explicit Send creates a bounded input event.
#![forbid(unsafe_code)]

use super::{
    ClientInstant, ControlledViewer, Granted, InputResult, Shared, control_policy, key_capture,
};
use frd::session_startup::viewer_events::{Error, MAX_EVENT_AGE_US, TextContext};
use std::time::Duration;

/// Opaque context captured before the first native edit. It grants no authority,
/// owns no text and cannot be rebound to a replacement input source or view.
/// No Debug or Clone: the shell consumes the draft on its one explicit Send.
pub struct TextDraft {
    context: TextContext,
}

/// One explicit Send, captured before any native text read or queue wait.
/// The original context and exclusive deadline survive admission unchanged.
/// No Debug or Clone: this value is consumed exactly once, including on refusal.
pub struct CapturedText {
    context: TextContext,
    capture: key_capture::Capture,
}

impl CapturedText {
    /// Charge the original native gesture's queue age. Sample the native clock
    /// after capture; neither conversion nor a later ticket may restamp Send.
    pub fn with_queue_age(mut self, age: Duration) -> Option<Self> {
        self.capture = self.capture.with_queue_age(age)?;
        Some(self)
    }
}

impl Shared {
    /// Begin one local composition under the current granted source and view.
    /// Unsupported text or held/pending input refuses without affecting keys.
    pub fn begin_text(&self) -> Result<TextDraft, InputResult> {
        let mut draft = None;
        let result = self.with_granted(|g| {
            if !g.source.supports_text() {
                return InputResult::Unsupported;
            }
            if held_or_queued(g) {
                return InputResult::Busy;
            }
            match g.source.text_context() {
                Ok(Some(context)) => {
                    draft = Some(TextDraft { context });
                    InputResult::Pending
                }
                Ok(None) => InputResult::Busy,
                Err(error) => control_policy::text_result(Err(error)),
            }
        });
        draft.ok_or(result)
    }

    /// Ok(false) is a temporary compositor gap or pending/held-input wait;
    /// preserve the local draft but disable Send. Any error invalidates the
    /// draft: identical later generations cannot make it current again.
    pub fn text_ready(&self, draft: &TextDraft) -> Result<bool, InputResult> {
        let mut ready = None;
        let result = self.with_granted(|g| match draft_ready(g, &draft.context) {
            Ok(value) => {
                ready = Some(value);
                InputResult::Pending
            }
            Err(result) => result,
        });
        ready.ok_or(result)
    }

    /// Consume the original local draft at Send before entering native APIs.
    /// A refusal consumes this choice too; the shell must clear its local text.
    pub fn capture_text(&self, draft: TextDraft) -> Result<CapturedText, InputResult> {
        let mut captured = None;
        let result = self.with_granted(|g| {
            match draft_ready(g, &draft.context) {
                Ok(true) => {}
                Ok(false) => return InputResult::Busy,
                Err(result) => return result,
            }
            let Ok(sampled) = g.source.clock() else {
                return InputResult::ControlLost;
            };
            let Some((context, until)) = &g.text_window else {
                return InputResult::Busy;
            };
            if *context != draft.context {
                return InputResult::Busy;
            }
            let Some(max_age) = until.0.checked_sub(sampled.0).filter(|age| *age != 0) else {
                return InputResult::Busy;
            };
            let Some(capture) = key_capture::Capture::new(
                &g.stop,
                g.opened.0,
                sampled.0,
                max_age.min(MAX_EVENT_AGE_US),
            ) else {
                return InputResult::Busy;
            };
            captured = Some(CapturedText {
                context: draft.context,
                capture,
            });
            InputResult::Pending
        });
        captured.ok_or(result)
    }

    /// Admit one whole Unicode value under the original native Send capture.
    /// Invalid text, changed context, expiry or pending input admits nothing and
    /// never retries. Sent names local queue admission, not host insertion.
    pub fn captured_text(&self, captured: CapturedText, text: &str) -> InputResult {
        if !control_policy::valid_text(text) {
            return InputResult::Invalid;
        }
        let CapturedText { context, capture } = captured;
        self.with_granted(move |g| {
            let Ok(now) = g.source.clock() else {
                return InputResult::ControlLost;
            };
            if !capture.current(&g.stop, now.0) || held_or_queued(g) {
                return InputResult::Busy;
            }
            g.scroll.reset();
            let (sampled, until) = capture.bounds();
            control_policy::text_result(g.source.commit_text_in_context_until(
                &context,
                text,
                ClientInstant(sampled),
                ClientInstant(until),
            ))
        })
    }
}

fn held_or_queued(g: &Granted) -> bool {
    // A contended queue is unknown, never evidence that earlier keys drained.
    g.source.queued() != Some(0)
        || g.keys.iter().any(|held| *held)
        || g.buttons.iter().any(|held| *held)
}

fn draft_ready(g: &Granted, context: &TextContext) -> Result<bool, InputResult> {
    match g.source.text_context_ready(context) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        // A genuine suspension changes the context even when the same view
        // resumes before the next native poll. It is not a readiness wait.
        Err(Error::StaleTextContext) => return Err(InputResult::Invalid),
        Err(error) => return Err(control_policy::text_result(Err(error))),
    }
    let Ok(now) = g.source.clock() else {
        return Err(InputResult::ControlLost);
    };
    Ok(!held_or_queued(g)
        && g.text_window
            .as_ref()
            .is_some_and(|(checked, until)| checked == context && now < *until))
}

/// Runs on the existing original viewer owner, outside the native UI. Sampling
/// checks/publishes any suspension before retaining this exact context and bound.
pub(super) fn refresh_window(viewer: &mut ControlledViewer, shared: &Shared) {
    let until = viewer.input_capture_context().ok().map(|(_, until)| until);
    let _ = shared.with_granted(|g| {
        g.text_window = until.and_then(|until| {
            g.source
                .text_context()
                .ok()
                .flatten()
                .map(|context| (context, until))
        });
        InputResult::Pending
    });
}
