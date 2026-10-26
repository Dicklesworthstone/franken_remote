//! `fr connect --control` spends the command-line control decision once.
//! A transient failure can reconnect viewing after confirmed native cleanup.
//! A fresh native F8 gesture may request a NEW lease on that recovered viewing
//! session; nothing retries a refused request or carries old input, held state,
//! files, clipboard consent or a lease into the new request.
//!
//! Visibility witness, stated rather than overclaimed: this X11 slice has no
//! compositor-independent optical proof. A frame is reported visible only when
//! the canonical presenter completed `SubmittedToCompositor` (image put and
//! synchronized into THIS window) while the fixed-size window is still
//! `Mapped`; unmapping already stops the session. Occlusion by another local
//! window is not detected, so completion keeps `physical_visibility_proven:
//! false`. A refused report grants nothing: the session keeps its own gates.
//!
//! The default-on controller-only UTF-8 clipboard is configured only while the
//! invocation still has its one explicit control request available, at
//! `ready`, before service: nothing opens until the grant and the host's own
//! readiness plus a mapped, responsive local Turn off panel. `--no-clipboard`
//! declines this optional lane. The X11 owner runs on the clipboard worker thread (this
//! local display, a separate XCB connection). Completion reports content-free
//! tallies only: whether it became active, how many host items were published
//! to the local CLIPBOARD, or the typed reason it never became active.
#[path = "control/renewal.rs"]
mod renewal;
use fr_client::input::{self, ResultEvent, viewport::SurfaceRect};
use fr_core::{
    input::{DesktopPoint, InputBounds},
    input_sequence::InputOutcome,
    input_submission::{Capabilities, Capability},
};
use fr_media::freshness::ClockPolicy;
use fr_native::{
    desktop::{
        Desktop,
        reconnect::{CleanupSummary, ControlPolicy},
    },
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

/// What this X11 viewer captures and wants from the `frd run --input-agent`
/// host profile: keys (with repeat), absolute pointer, buttons and discrete
/// line scrolling on both axes. Line scrolling is requested only when the host
/// selected the optional `native-input-line-scroll` boundary (its executor can
/// scroll by lines); otherwise the wheel alone is unavailable. Pixel scrolling,
/// relative pointer and text stay unavailable; no fractional wheel
/// accumulation or keyboard emulation is used.
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

/// The local half of shared clipboard: this user's X11 display (the one the
/// viewer window uses) and the runtime's qualified randomness for item IDs.
#[derive(Clone)]
// Without linux-clipboard, configuration reports this optional lane unavailable.
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
    /// Why an ACTIVE clipboard retired before the session ended (typed code).
    /// Control continues; this lane is not re-attached under the same grant.
    pub(super) ended: Option<&'static str>,
}
impl ClipboardCounters {
    /// The typed reason reported when the lane never became active.
    pub(super) fn absence(&self) -> Option<&'static str> {
        (!self.active).then(|| self.absence.unwrap_or("clipboard_not_started"))
    }
}
/// Typed code for an active clipboard that retired mid-session.
fn ended(reason: Option<frd::clipboard_quic::Error>) -> &'static str {
    use fr_core::clipboard::Error as Item;
    use fr_wire::clipboard::session::SessionError;
    use frd::clipboard_quic::Error;
    match reason {
        Some(Error::HandoffExpired | Error::Session(SessionError::Clipboard(Item::Expired))) => {
            "clipboard_item_expired"
        }
        Some(Error::SwitchChanged | Error::Session(SessionError::Clipboard(Item::Disabled))) => {
            "clipboard_disabled"
        }
        Some(
            Error::Controller(_) | Error::Session(SessionError::Clipboard(Item::Authority(_))),
        ) => "clipboard_authority_ended",
        _ => "clipboard_ended",
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
    frd::native_clipboard::Configuration::new_with_control(
        // Local connection policy enables text sharing unless declined with
        // --no-clipboard. Desktop gates native access on its visible off panel.
        true,
        std::time::Duration::from_secs(2),
        move |original| {
            fr_native::clipboard::X11Clipboard::open(
                &display,
                &fr_core::limits::ProtocolLimits::ABSOLUTE,
                true,
            )
            .map(|native| native.with_privacy_gate(move || original.status().enabled))
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

/// Content-free names of granted operations, in a fixed order.
pub(super) fn capability_names(granted: Capabilities) -> Vec<&'static str> {
    [
        (Capability::Keys, "keys"),
        (Capability::Repeat, "repeat"),
        (Capability::Absolute, "absolute_pointer"),
        (Capability::Buttons, "buttons"),
        (Capability::Relative, "relative_pointer"),
        (Capability::PixelScroll, "pixel_scroll"),
        (Capability::LineScroll, "line_scroll"),
        (Capability::Text, "text"),
    ]
    .into_iter()
    .filter(|(c, _)| granted.contains(*c))
    .map(|(_, name)| name)
    .collect()
}
impl Counters {
    /// Granted, but without the wheel: the host's input executor cannot
    /// scroll by lines (several X screens, or wheel buttons unmapped).
    pub(super) fn wheel_unavailable(&self) -> bool {
        self.capabilities
            .is_some_and(|c| !c.contains(Capability::LineScroll))
    }
}

/// Content-free tallies for the completion report; never input values.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Counters {
    pub(super) requested: bool,
    pub(super) granted: bool,
    /// The host's exact grant, once granted (never clamped by the host).
    pub(super) capabilities: Option<Capabilities>,
    pub(super) results: u64,
    pub(super) submitted: u64,
    /// Stale-view suspensions of control input and their total duration.
    pub(super) suspensions: u32,
    pub(super) suspended_us: u64,
    /// Captured local events dropped as obsolete after aging past their
    /// dispatch bound (counts only).
    pub(super) dropped_aged: u64,
    /// The original request-bearing attempt's outcome, frozen at its cleanup.
    /// Later viewing-only attempts cannot replace this evidence.
    pub(super) ended: Option<Result<(), Failure>>,
    /// Intent for the current connection attempt, never a claim of authority.
    pub(super) mode: AttemptMode,
    /// Fresh viewing-only desktops actually opened after the request was spent.
    pub(super) viewing_reconnects: u8,
    pub(super) current_attempt: u8,
    pub(super) request_attempt: Option<u8>,
    /// Original request-bearing native owner, including late input-stop causes.
    pub(super) cleanup: Option<CleanupSummary>,
    /// Some when this controller's clipboard policy is enabled.
    pub(super) clipboard: Option<ClipboardCounters>,
    /// One slot per bounded connection attempt. Original report fields above
    /// are never repurposed when the user makes another explicit local choice.
    pub(super) fresh: [Option<renewal::Fresh>; renewal::MAX_ATTEMPTS],
}
impl Counters {
    pub(super) fn begin_attempt(&mut self, attempt: u8) {
        self.current_attempt = attempt;
        self.mode = if self.requested {
            AttemptMode::ViewingOnly
        } else {
            AttemptMode::Control
        };
    }
    pub(super) fn opened(&mut self) -> AttemptMode {
        if self.mode == AttemptMode::ViewingOnly {
            self.viewing_reconnects = self.viewing_reconnects.saturating_add(1);
        }
        self.mode
    }
    /// Spend the local decision before asking the session to admit it. Even an
    /// uncertain or refused request cannot authorize another one on reconnect.
    pub(super) fn spend_request(&mut self) {
        self.requested = true;
        self.request_attempt.get_or_insert(self.current_attempt);
    }
    pub(super) fn retain_cleanup(&mut self, attempt: u8, summary: CleanupSummary) {
        if self.request_attempt == Some(attempt) {
            self.cleanup = Some(summary);
        } else if let Some(fresh) = self
            .fresh
            .iter_mut()
            .flatten()
            .find(|f| f.attempt == attempt)
        {
            fresh.cleanup = Some(summary);
        }
    }
    /// The fresh local decision is spent before wire admission, just as the
    /// command-line decision was. Neither outcome can authorize an automatic retry.
    pub(super) fn spend_fresh_request(&mut self) -> Result<(), CallbackError> {
        if self.mode != AttemptMode::ViewingOnly
            || self
                .request_attempt
                .is_none_or(|first| self.current_attempt <= first)
        {
            return Err(CallbackError);
        }
        let slot = self
            .current_attempt
            .checked_sub(1)
            .and_then(|n| self.fresh.get_mut(usize::from(n)))
            .ok_or(CallbackError)?;
        if slot.is_some() {
            return Err(CallbackError);
        }
        *slot = Some(renewal::Fresh::new(self.current_attempt));
        self.mode = AttemptMode::ExplicitControl;
        Ok(())
    }
    fn current_fresh(&mut self) -> Option<&mut renewal::Fresh> {
        self.fresh
            .iter_mut()
            .flatten()
            .find(|f| f.attempt == self.current_attempt)
    }
    pub(super) fn latest_cleanup(&self) -> Option<CleanupSummary> {
        self.fresh
            .iter()
            .rev()
            .flatten()
            .find_map(|f| f.cleanup)
            .or(self.cleanup)
    }
    fn control_metrics(&mut self, viewer: &ControlledViewer) -> Result<(), CallbackError> {
        let capabilities = viewer.granted_capabilities();
        let (suspensions, suspended_us) = viewer.suspension_totals();
        let dropped_aged = viewer.input_dropped_aged();
        if let Some(fresh) = self.current_fresh() {
            fresh.granted = true;
            fresh.capabilities = Some(capabilities);
            fresh.suspensions = suspensions;
            fresh.suspended_us = suspended_us;
            fresh.dropped_aged = dropped_aged;
        } else if self.request_attempt == Some(self.current_attempt) {
            self.granted = true;
            self.capabilities = Some(capabilities);
            self.suspensions = suspensions;
            self.suspended_us = suspended_us;
            self.dropped_aged = dropped_aged;
        } else {
            return Err(CallbackError);
        }
        Ok(())
    }
    pub(super) fn retry_stopped(&self) -> bool {
        self.latest_cleanup().is_some_and(|summary| {
            summary.input_stop.is_some_and(|reason| {
                !matches!(
                    reason,
                    fr_native::viewer_input::StopReason::Local
                        | fr_native::viewer_input::StopReason::Closed
                )
            }) || summary
                .disconnect
                .is_some_and(|outcome| outcome.report.is_some())
                || summary.control_disconnect.is_some_and(|outcome| {
                    outcome.revocation.is_some() || outcome.exchange.report.is_some()
                })
        })
    }
    /// Count host-reported results by stage only. Duplicates are not new work.
    pub(super) fn result(&mut self, event: &ResultEvent) {
        if let Some(fresh) = self.current_fresh() {
            fresh.result(event);
            return;
        }
        if self.request_attempt != Some(self.current_attempt) {
            return;
        }
        if let ResultEvent::Completed(result) | ResultEvent::Pointer(result) = event {
            self.results = self.results.saturating_add(1);
            if matches!(result.outcome, InputOutcome::SubmittedToOs) {
                self.submitted = self.submitted.saturating_add(1);
            }
        }
    }
    /// Retain the first request-bearing attempt's own outcome. The canonical
    /// supervisor still owns error classification and cleanup-before-retry;
    /// allowing its retry restores viewing only, never another request.
    pub(super) fn cleaning(&mut self, failure: Option<Failure>) {
        if let Some(fresh) = self.current_fresh() {
            fresh.ended.get_or_insert(failure.map_or(Ok(()), Err));
        } else if self.request_attempt == Some(self.current_attempt) {
            self.ended.get_or_insert(failure.map_or(Ok(()), Err));
        }
    }
}

/// The command-line decision is consumed once, even when its reply was lost.
/// Every recovery starts viewing; only a fresh native gesture can change that.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum AttemptMode {
    #[default]
    Control,
    ViewingOnly,
    ExplicitControl,
}
impl AttemptMode {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Control => "control_capable",
            Self::ViewingOnly => "view_only",
            Self::ExplicitControl => "explicit_control",
        }
    }
}

/// One opened attempt's local state, created at `ready` and never reused.
pub(super) struct Attempt {
    mode: AttemptMode,
    display: Display,
    window: WindowControl,
    mapped: bool,
    shown: Option<u64>,
    attached: bool,
    /// This attempt's content-free clipboard handle; never carried over.
    clipboard: Option<frd::native_clipboard::Control>,
}
impl Attempt {
    pub(super) fn new(desktop: &Desktop, mode: AttemptMode) -> Option<Self> {
        let window = desktop.window()?;
        if mode == AttemptMode::ViewingOnly {
            window.enable_control_request().ok()?;
        }
        Some(Self {
            mode,
            display: desktop.display()?,
            window,
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
            // Retired, not Closed: the lane ended while the session ran on.
            Phase::Retired => {
                counters.ended.get_or_insert(ended(status.reason));
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
        if self.mode == AttemptMode::ViewingOnly
            && !matches!(&state, InteractiveViewerState::Viewing(_))
        {
            // No request was made on this connection. Never accept a stray
            // grant, configure input capture, or inherit the old layout.
            return Err(CallbackError);
        }
        if let Some(clipboard) = counters.clipboard.as_mut() {
            self.clipboard_turn(clipboard);
        }
        match state {
            InteractiveViewerState::Viewing(view) => {
                let submitted = view
                    .presentation()
                    .is_some_and(|p| p.stage == PresentationStage::SubmittedToCompositor);
                if !submitted || self.window.status() != WindowStatus::Mapped {
                    self.window.take_control_request(false);
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
                let ready = view.evidence().is_ok();
                if self.mode == AttemptMode::Control && !counters.requested && ready {
                    counters.spend_request();
                    view.request_control().map_err(|_| CallbackError)?;
                } else if self.mode == AttemptMode::ViewingOnly
                    && self.window.take_control_request(ready)
                {
                    counters.spend_fresh_request()?;
                    self.mode = AttemptMode::ExplicitControl;
                    view.request_control().map_err(|_| CallbackError)?;
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
                counters.control_metrics(viewer)?;
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
    use fr_wire::{attachment, negotiation::Role};
    #[cfg(not(feature = "linux-clipboard"))]
    #[test]
    fn absent_native_clipboard_is_optional_with_a_typed_local_refusal() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let local = Clipboard {
            display: ":0".into(),
            cx: runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE),
        };
        let error = configuration(&local).unwrap_err();
        assert_eq!(
            error,
            frd::clipboard_quic::Error::NativeSetup(fr_core::clipboard::PlatformError::Unsupported)
        );
        assert_eq!(absence(Some(error)), "local_clipboard_unavailable");
        assert!(
            offer(true)
                .capabilities
                .iter()
                .filter(|cap| cap.name.contains("clipboard"))
                .all(|cap| !cap.required)
        );
    }
    #[test]
    fn the_control_offer_and_capabilities_match_the_x11_host_profile_only() {
        let offer = offer(false);
        assert!(offer.validate().is_ok());
        assert_eq!(offer.role, Role::RequestControl);
        assert_eq!(offer, fr_client::native::control_offer());
        assert!(offer.capabilities.iter().any(|cap| {
            cap.name == fr_wire::input_ticket::AUTHORITY_CAPABILITY
                && cap.version == fr_wire::input_ticket::AUTHORITY_VERSION
                && !cap.required
        }));
        assert!(offer.capabilities.iter().any(|cap| {
            cap.name == fr_wire::input_ticket::READINESS_CAPABILITY
                && cap.version == fr_wire::input_ticket::READINESS_VERSION
                && !cap.required
        }));
        // --clipboard adds three optional clipboard boundaries and the optional
        // bulk-record extension; the required capability set stays unchanged.
        let clipboard = super::offer(true);
        assert!(clipboard.validate().is_ok());
        assert_eq!(clipboard.role, Role::RequestControl);
        assert_eq!(clipboard.capabilities.len(), offer.capabilities.len() + 4);
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
        assert!(
            offer
                .capabilities
                .iter()
                .all(|c| c.name != attachment::BULK_RECORD_CAPABILITY)
        );
        assert!(clipboard.capabilities.iter().any(|c| {
            c.name == attachment::BULK_RECORD_CAPABILITY
                && c.version == attachment::BULK_RECORD_VERSION
                && !c.required
        }));
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
    fn an_active_clipboard_that_retires_names_why() {
        use fr_core::clipboard::Error as Item;
        use fr_wire::clipboard::session::SessionError;
        use frd::clipboard_quic::Error;
        for (reason, code) in [
            (Error::HandoffExpired, "clipboard_item_expired"),
            (
                Error::Session(SessionError::Clipboard(Item::Expired)),
                "clipboard_item_expired",
            ),
            (Error::SwitchChanged, "clipboard_disabled"),
            (
                Error::Session(SessionError::Clipboard(Item::Authority(
                    fr_core::input_submission::Refusal::Revoked,
                ))),
                "clipboard_authority_ended",
            ),
            (
                Error::Session(SessionError::Clipboard(Item::LocalChanged)),
                "clipboard_ended",
            ),
            (Error::Closed, "clipboard_ended"),
        ] {
            assert_eq!(ended(Some(reason)), code, "{reason:?}");
        }
        assert_eq!(ended(None), "clipboard_ended");
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
    fn the_spent_request_survives_uncertainty_and_later_viewing_cleanup() {
        let mut counters = Counters::default();
        counters.begin_attempt(1);
        assert_eq!(counters.opened(), AttemptMode::Control);
        counters.cleaning(Some(Failure::Cancelled));
        assert_eq!(counters.ended, None, "no control request was spent");
        counters.begin_attempt(2);
        assert_eq!(counters.opened(), AttemptMode::Control);
        counters.spend_request();
        // The request is already spent if admission or its reply now fails.
        assert!(counters.requested);
        assert_eq!(counters.request_attempt, Some(2));
        counters.cleaning(Some(Failure::Cancelled));
        assert_eq!(counters.ended, Some(Err(Failure::Cancelled)));
        counters.results = 3;
        counters.submitted = 2;
        for attempt in [3, 4] {
            counters.begin_attempt(attempt);
            assert_eq!(counters.opened(), AttemptMode::ViewingOnly);
            counters.cleaning(None);
            assert_eq!(counters.request_attempt, Some(2));
            assert_eq!(counters.ended, Some(Err(Failure::Cancelled)));
            assert_eq!((counters.results, counters.submitted), (3, 2));
        }
        assert_eq!(counters.viewing_reconnects, 2);
    }
    #[test]
    fn only_the_original_request_owner_can_supply_cleanup_evidence() {
        use fr_native::viewer_input::StopReason;
        let mut counters = Counters::default();
        counters.begin_attempt(2);
        counters.spend_request();
        let mut summary = CleanupSummary {
            disconnect: None,
            control_disconnect: None,
            input_stop: Some(StopReason::FocusLost),
            failure: None,
        };
        counters.retain_cleanup(1, summary);
        assert_eq!(counters.cleanup, None);
        counters.retain_cleanup(2, summary);
        assert!(counters.retry_stopped());
        counters.begin_attempt(3);
        summary.input_stop = Some(StopReason::Local);
        counters.retain_cleanup(3, summary);
        assert_eq!(
            counters.cleanup.unwrap().input_stop,
            Some(StopReason::FocusLost)
        );
        assert!(counters.retry_stopped());
    }
    #[test]
    fn cleanup_generated_stops_allow_viewing_but_native_stop_intent_is_terminal() {
        use fr_native::viewer_input::StopReason;
        for reason in [
            None,
            Some(StopReason::Local),
            Some(StopReason::Closed),
            Some(StopReason::FocusLost),
            Some(StopReason::LocalEscape),
            Some(StopReason::WindowChanged),
            Some(StopReason::Expired),
        ] {
            let mut counters = Counters::default();
            counters.begin_attempt(1);
            counters.spend_request();
            counters.retain_cleanup(
                1,
                CleanupSummary {
                    disconnect: None,
                    control_disconnect: None,
                    input_stop: reason,
                    failure: None,
                },
            );
            assert_eq!(
                counters.retry_stopped(),
                !matches!(reason, None | Some(StopReason::Local | StopReason::Closed)),
                "{reason:?}"
            );
        }
    }
    fn result_event(lease: u128) -> ResultEvent {
        use fr_core::ids::{InputLeaseId, RemoteSessionId};
        use fr_wire::input_result::{InputResult, ResultBinding, SequenceSpace, Stage};
        ResultEvent::Completed(InputResult {
            binding: ResultBinding {
                channel: 7,
                session: RemoteSessionId::from_raw(9),
                lease: InputLeaseId::from_raw(lease),
            },
            sequence: 1,
            space: SequenceSpace::Action,
            stage: Stage::SubmittedToOs,
            outcome: InputOutcome::SubmittedToOs,
            submitted_operations: 1,
            unknown_next_operation: false,
            reason: None,
        })
    }

    #[test]
    fn a_fresh_choice_keeps_original_effects_and_terminal_cleanup_separate() {
        use fr_native::viewer_input::StopReason;
        let mut counters = Counters::default();
        counters.begin_attempt(1);
        assert_eq!(counters.spend_fresh_request(), Err(CallbackError));
        counters.spend_request();
        counters.granted = true;
        counters.result(&result_event(101));
        let original = CleanupSummary {
            disconnect: None,
            control_disconnect: None,
            input_stop: Some(StopReason::Local),
            failure: None,
        };
        counters.retain_cleanup(1, original);
        counters.cleaning(Some(Failure::Cancelled));

        counters.begin_attempt(2);
        assert_eq!(counters.opened(), AttemptMode::ViewingOnly);
        // Unrequested later result callbacks cannot mutate the original report.
        counters.result(&result_event(202));
        assert_eq!((counters.results, counters.submitted), (1, 1));
        counters.spend_fresh_request().unwrap();
        assert_eq!(counters.spend_fresh_request(), Err(CallbackError));
        counters.result(&result_event(202));
        counters.result(&ResultEvent::Unretained);
        let fresh_cleanup = CleanupSummary {
            input_stop: Some(StopReason::FocusLost),
            ..original
        };
        counters.retain_cleanup(2, fresh_cleanup);
        counters.cleaning(None);
        assert!(counters.retry_stopped());
        assert_eq!(counters.latest_cleanup(), Some(fresh_cleanup));
        assert_eq!(counters.cleanup, Some(original));
        assert_eq!(counters.request_attempt, Some(1));
        assert_eq!(counters.ended, Some(Err(Failure::Cancelled)));
        assert_eq!((counters.results, counters.submitted), (1, 1));
        let fresh = counters.current_fresh().unwrap();
        assert_eq!((fresh.results, fresh.submitted), (1, 1));
        assert_eq!(fresh.ended, Some(Ok(())));
        let json: serde_json::Value = serde_json::from_str(&format!(
            "{{\"original\":true{}}}",
            counters.fresh_history().json
        ))
        .unwrap();
        assert_eq!(json["explicit_control_requests"][0]["attempt"], 2);
        assert_eq!(
            json["explicit_control_requests"][0]["input_submitted_to_os"],
            1
        );
    }

    #[test]
    fn fresh_request_history_is_bounded_by_the_real_reconnect_policy() {
        use frd::native_connection::reconnect::Policy;
        let mut counters = Counters::default();
        counters.begin_attempt(1);
        counters.spend_request();
        for attempt in 2..=32 {
            counters.begin_attempt(attempt);
            assert_eq!(counters.opened(), AttemptMode::ViewingOnly);
            counters.spend_fresh_request().unwrap();
            assert_eq!(counters.mode, AttemptMode::ExplicitControl);
            counters.cleaning(Some(Failure::Cancelled));
        }
        assert_eq!(counters.fresh.iter().flatten().count(), 31);
        assert!(
            Policy {
                max_attempts: 32,
                ..Policy::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            Policy {
                max_attempts: 33,
                ..Policy::default()
            }
            .validate()
            .is_err()
        );
        counters.begin_attempt(33);
        assert_eq!(counters.spend_fresh_request(), Err(CallbackError));
        assert_eq!(counters.request_attempt, Some(1));
    }
}
