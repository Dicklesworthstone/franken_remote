//! `fr connect --control`: at most ONE explicit control request per `fr`
//! invocation. Passing `--control` is the local user's decision; nothing here
//! retries a refused or expired request, reacquires control after loss, or
//! carries input, a lease or held state into a later connection attempt.
//!
//! Visibility witness, stated rather than overclaimed: this X11 slice has no
//! compositor-independent optical proof. A frame is reported visible only when
//! the canonical presenter completed `SubmittedToCompositor` (image put and
//! synchronized into THIS window) while the fixed-size window is still
//! `Mapped`; unmapping already stops the session. Occlusion by another local
//! window is not detected, so completion keeps `physical_visibility_proven:
//! false`. A refused report grants nothing: the session keeps its own gates.
//!
//! `--clipboard` configures each attempt's controller-only UTF-8 clipboard at
//! `ready`, before service: nothing opens until the grant and the host's own
//! readiness; the X11 owner then runs on the clipboard worker thread (this
//! local display, a separate XCB connection). Completion reports content-free
//! tallies only: whether it became active, how many host items were published
//! to the local CLIPBOARD, or the typed reason it never became active.
use fr_client::input::{self, ResultEvent, viewport::SurfaceRect};
use fr_core::{
    input::{DesktopPoint, InputBounds},
    input_sequence::InputOutcome,
    input_submission::{Capabilities, Capability},
};
use fr_media::freshness::ClockPolicy;
use fr_native::{
    desktop::{Desktop, reconnect::ControlPolicy},
    viewer_window::{Status as WindowStatus, WindowControl},
};
use fr_wire::{display::Display, negotiation::Offer};
use frd::{
    media::PresentationStage,
    native_connection::reconnect::{CallbackError, Failure},
    session_startup::{
        ControlledViewer, ControlledViewerError, InteractiveViewerState, Presentation,
        StreamingViewerError, viewer_events::Layout,
    },
};

/// Exactly what this X11 viewer captures and the `frd run --input-agent` host
/// profile executes: keys (with repeat), absolute pointer, buttons and discrete
/// line scrolling on both axes. Pixel scrolling, relative pointer and text stay
/// unavailable; no fractional wheel accumulation or keyboard emulation is used.
pub(super) fn capabilities() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
}
pub(super) fn policy() -> ControlPolicy {
    ControlPolicy {
        clock: ClockPolicy::default(),
        input: input::Policy::default(),
        sequence: 1,
        capabilities: capabilities(),
    }
}
pub(super) fn offer(clipboard: bool) -> Offer {
    if clipboard {
        fr_client::native::control_offer_with_clipboard()
    } else {
        fr_client::native::control_offer()
    }
}

/// The local half of `--clipboard`: this user's X11 display (the one the
/// viewer window uses) and the runtime's qualified randomness for item IDs.
#[derive(Clone)]
// Without linux-clipboard, `connect` refuses --clipboard before building one.
#[cfg_attr(not(feature = "linux-clipboard"), allow(dead_code))]
pub(super) struct Clipboard {
    pub(super) display: String,
    pub(super) cx: asupersync::cx::Cx,
}
/// Content-free clipboard tallies for the completion report; never text.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClipboardCounters {
    /// The lane reached `Running`: bilateral readiness completed and this
    /// side's worker started (not proof that either native owner opened).
    pub(super) active: bool,
    /// Host items committed to THIS display's CLIPBOARD (`SubmittedToOs`).
    pub(super) received: u64,
    /// Why it never became active, when known (typed code).
    pub(super) absence: Option<&'static str>,
}
impl ClipboardCounters {
    /// The typed reason reported when the lane never became active.
    pub(super) fn absence(&self) -> Option<&'static str> {
        (!self.active).then(|| self.absence.unwrap_or("clipboard_not_started"))
    }
}
/// Typed absence code for a clipboard that ended without becoming active.
fn absence(reason: Option<frd::clipboard_quic::Error>) -> &'static str {
    use frd::clipboard_quic::Error;
    match reason {
        Some(Error::NotNegotiated) => "host_clipboard_unavailable",
        Some(Error::ConsentRequired) => "host_clipboard_refused",
        Some(Error::NativeSetup(_)) => "local_clipboard_unavailable",
        _ => "clipboard_ended",
    }
}
#[cfg(feature = "linux-clipboard")]
fn configuration(
    local: &Clipboard,
) -> Result<frd::native_clipboard::Configuration, frd::clipboard_quic::Error> {
    use fr_wire::clipboard::session::synchronize::IdentifierFailure;
    let display = local.display.clone();
    let cx = local.cx.clone();
    frd::native_clipboard::Configuration::new(
        // The local user's explicit `--clipboard` is this side's consent.
        true,
        std::time::Duration::from_secs(2),
        move || {
            fr_native::clipboard::X11Clipboard::open(
                &display,
                &fr_core::limits::ProtocolLimits::ABSOLUTE,
                true,
            )
        },
        move || {
            let mut bytes = [0; 16];
            cx.random_bytes(&mut bytes);
            let id = u128::from_be_bytes(bytes);
            if id == 0 {
                Err(IdentifierFailure)
            } else {
                Ok(id)
            }
        },
    )
}
#[cfg(not(feature = "linux-clipboard"))]
fn configuration(
    _: &Clipboard,
) -> Result<frd::native_clipboard::Configuration, frd::clipboard_quic::Error> {
    Err(frd::clipboard_quic::Error::NativeSetup(
        fr_core::clipboard::PlatformError::Unsupported,
    ))
}

/// Content-free tallies for the completion report; never input values.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Counters {
    pub(super) requested: bool,
    pub(super) granted: bool,
    pub(super) results: u64,
    pub(super) submitted: u64,
    /// The ORIGINAL outcome of the attempt whose cleanup ended reconnection.
    pub(super) ended: Option<Result<(), Failure>>,
    /// Some only with `--clipboard`.
    pub(super) clipboard: Option<ClipboardCounters>,
}
impl Counters {
    /// Count host-reported results by stage only. Duplicates are not new work.
    pub(super) fn result(&mut self, event: &ResultEvent) {
        if let ResultEvent::Completed(result) | ResultEvent::Pointer(result) = event {
            self.results = self.results.saturating_add(1);
            if matches!(result.outcome, InputOutcome::SubmittedToOs) {
                self.submitted = self.submitted.saturating_add(1);
            }
        }
    }
    /// At each attempt's cleanup. Once a request was made, a reconnect that
    /// asked again would be automatic reacquisition: refuse further attempts
    /// AFTER mandatory cleanup and retain the attempt's own outcome.
    pub(super) fn cleaning(&mut self, failure: Option<Failure>) -> Result<(), CallbackError> {
        if !self.requested {
            return Ok(());
        }
        self.ended = Some(failure.map_or(Ok(()), Err));
        Err(CallbackError)
    }
}

/// One opened attempt's local state, created at `ready` and never reused.
pub(super) struct Attempt {
    display: Display,
    window: WindowControl,
    mapped: bool,
    shown: Option<u64>,
    attached: bool,
    /// This attempt's content-free clipboard handle; never carried over.
    clipboard: Option<frd::native_clipboard::Control>,
}
impl Attempt {
    pub(super) fn new(desktop: &Desktop) -> Option<Self> {
        Some(Self {
            display: desktop.display()?,
            window: desktop.window()?,
            mapped: false,
            shown: None,
            attached: false,
            clipboard: None,
        })
    }
    /// Configure this attempt's controller-only clipboard before service. A
    /// host without clipboard (not negotiated) or a local setup refusal is a
    /// typed absence; control itself is unaffected either way.
    pub(super) fn configure_clipboard(
        &mut self,
        desktop: &mut Desktop,
        local: &Clipboard,
        counters: &mut ClipboardCounters,
    ) {
        let configured = configuration(local).and_then(|configuration| {
            desktop
                .configure_clipboard(configuration)
                .map_err(|error| match error {
                    fr_native::desktop::Error::Clipboard(error) => error,
                    _ => frd::clipboard_quic::Error::Closed,
                })
        });
        match configured {
            Ok(control) => self.clipboard = Some(control),
            Err(error) => counters.absence = Some(absence(Some(error))),
        }
    }
    /// Sample the clipboard handle: count committed host items, note activity
    /// and a typed end. Draining also frees the handle's two receipt slots.
    pub(super) fn clipboard_turn(&self, counters: &mut ClipboardCounters) {
        use fr_core::clipboard::Publication;
        use fr_wire::clipboard::session::synchronize::Received;
        use frd::native_clipboard::Phase;
        let Some(control) = &self.clipboard else {
            return;
        };
        while let Some(receipt) = control.take_received() {
            if matches!(receipt, Received::Consumed(Some(r)) if r.publication == Publication::SubmittedToOs)
            {
                counters.received = counters.received.saturating_add(1);
            }
        }
        let status = control.status();
        match status.phase {
            Phase::Running => counters.active = true,
            Phase::Retired | Phase::Closed if !counters.active => {
                counters.absence = Some(absence(status.reason));
            }
            _ => {}
        }
    }
    /// The newest X11-submitted completion not yet reported, only while the
    /// window is mapped. Decode-only completions are never reported visible.
    fn candidate(&self, latest: Option<Presentation>) -> Option<u64> {
        let latest = latest?;
        let frame = latest.frame.as_raw();
        (latest.stage == PresentationStage::SubmittedToCompositor
            && self.shown != Some(frame)
            && self.window.status() == WindowStatus::Mapped)
            .then_some(frame)
    }
    /// A clock that is not yet correlated leaves the frame reportable on a
    /// later turn; any other refusal waits for a newer completion.
    fn witness<T>(&mut self, frame: u64, result: &Result<T, StreamingViewerError>) {
        if !matches!(
            result,
            Err(StreamingViewerError::Control(
                ControlledViewerError::ClockNotReady
            ))
        ) {
            self.shown = Some(frame);
        }
    }
    /// Viewing: confirm this window's mapping once it shows the selected
    /// display, report X11-submitted frames, then request control ONCE when the
    /// session itself reports current evidence. Requesting: keep reporting.
    /// Controlled: supply the one confirmed layout that attaches X11 capture.
    pub(super) fn turn(
        &mut self,
        state: InteractiveViewerState<'_>,
        frame: Option<Presentation>,
        counters: &mut Counters,
    ) -> Result<Option<Layout>, CallbackError> {
        if let Some(clipboard) = counters.clipboard.as_mut() {
            self.clipboard_turn(clipboard);
        }
        match state {
            InteractiveViewerState::Viewing(view) => {
                let submitted = view
                    .presentation()
                    .is_some_and(|p| p.stage == PresentationStage::SubmittedToCompositor);
                if !submitted || self.window.status() != WindowStatus::Mapped {
                    return Ok(None);
                }
                if !self.mapped {
                    let request = view.request();
                    view.confirm_mapping(request.parent, request.target.view)
                        .map_err(|_| CallbackError)?;
                    self.mapped = true;
                }
                if let Some(frame) = self.candidate(view.presentation()) {
                    let result = view.visible(frame);
                    self.witness(frame, &result);
                }
                if !counters.requested && view.evidence().is_ok() {
                    view.request_control().map_err(|_| CallbackError)?;
                    counters.requested = true;
                }
                Ok(None)
            }
            InteractiveViewerState::Requesting(pending) => {
                if let Some(frame) = self.candidate(pending.presentation()) {
                    let result = pending.visible(frame);
                    self.witness(frame, &result);
                }
                Ok(None)
            }
            InteractiveViewerState::Controlled(viewer) => {
                counters.granted = true;
                if let Some(frame) = self.candidate(frame) {
                    // The controlled clock is already correlated; a refusal
                    // only withholds evidence and new input stays gated.
                    let _ = viewer.visible(frame);
                    self.shown = Some(frame);
                }
                if self.attached {
                    return Ok(None);
                }
                let layout = self.layout(viewer)?;
                self.attached = true;
                Ok(Some(layout))
            }
        }
    }
    /// The whole selected display into this exact window: 1:1 for native
    /// pixels, the same centered aspect fit as the `--fit` presenter otherwise.
    /// `Desktop::serve_interactive` independently rechecks both rectangles.
    fn layout(&self, viewer: &mut ControlledViewer) -> Result<Layout, CallbackError> {
        let window = self.window.input_window().map_err(|_| CallbackError)?;
        let source = InputBounds::new(
            DesktopPoint {
                x: self.display.x,
                y: self.display.y,
            },
            self.display.pixel_width,
            self.display.pixel_height,
        )
        .ok_or(CallbackError)?;
        let area =
            SurfaceRect::new(0, 0, window.width, window.height).map_err(|_| CallbackError)?;
        let layout = viewer
            .configure_viewport(source, area)
            .map_err(|_| CallbackError)?;
        viewer
            .confirm_viewport(&layout)
            .map_err(|_| CallbackError)?;
        Ok(layout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_wire::negotiation::Role;
    #[test]
    fn the_control_offer_and_capabilities_match_the_x11_host_profile_only() {
        let offer = offer(false);
        assert!(offer.validate().is_ok());
        assert_eq!(offer.role, Role::RequestControl);
        assert_eq!(offer, fr_client::native::control_offer());
        // --clipboard adds only the three OPTIONAL clipboard boundaries.
        let clipboard = super::offer(true);
        assert!(clipboard.validate().is_ok());
        assert_eq!(clipboard.role, Role::RequestControl);
        assert_eq!(clipboard.capabilities.len(), offer.capabilities.len() + 3);
        assert_eq!(
            clipboard.capabilities.iter().filter(|c| c.required).count(),
            offer.capabilities.iter().filter(|c| c.required).count()
        );
        assert!(
            clipboard
                .capabilities
                .iter()
                .filter(|c| c.name.contains("clipboard"))
                .all(|c| !c.required)
        );
        let caps = capabilities();
        for granted in [
            Capability::Keys,
            Capability::Repeat,
            Capability::Absolute,
            Capability::Buttons,
            Capability::LineScroll,
        ] {
            assert!(caps.contains(granted));
        }
        for absent in [
            Capability::Text,
            Capability::PixelScroll,
            Capability::Relative,
        ] {
            assert!(!caps.contains(absent));
        }
        let policy = policy();
        assert_eq!(policy.sequence, 1);
        assert_eq!(policy.capabilities, caps);
    }
    #[test]
    fn clipboard_absence_is_typed_and_never_reported_once_active() {
        use frd::clipboard_quic::Error;
        assert_eq!(
            absence(Some(Error::NotNegotiated)),
            "host_clipboard_unavailable"
        );
        assert_eq!(
            absence(Some(Error::ConsentRequired)),
            "host_clipboard_refused"
        );
        assert_eq!(
            absence(Some(Error::NativeSetup(
                fr_core::clipboard::PlatformError::Unavailable
            ))),
            "local_clipboard_unavailable"
        );
        assert_eq!(absence(None), "clipboard_ended");
        let mut counters = ClipboardCounters::default();
        assert_eq!(counters.absence(), Some("clipboard_not_started"));
        counters.absence = Some("host_clipboard_unavailable");
        assert_eq!(counters.absence(), Some("host_clipboard_unavailable"));
        counters.active = true;
        assert_eq!(counters.absence(), None);
    }
    #[test]
    fn a_made_request_ends_reconnection_and_keeps_the_original_outcome() {
        let mut counters = Counters::default();
        assert_eq!(counters.cleaning(Some(Failure::Cancelled)), Ok(()));
        assert_eq!(
            counters.ended, None,
            "no request: ordinary reconnect policy"
        );
        counters.requested = true;
        assert_eq!(
            counters.cleaning(Some(Failure::Cancelled)),
            Err(CallbackError)
        );
        assert_eq!(counters.ended, Some(Err(Failure::Cancelled)));
        assert_eq!(counters.cleaning(None), Err(CallbackError));
        assert_eq!(counters.ended, Some(Ok(())));
    }
}
