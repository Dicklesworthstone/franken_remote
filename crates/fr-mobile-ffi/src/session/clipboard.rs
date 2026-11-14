//! One Windows controller's original optional clipboard owner. Native text
//! stays in the existing bounded worker; the app retains only status and
//! publication receipts, including after stop or failed cleanup.
#![forbid(unsafe_code)]

use asupersync::cx::Cx;
use fr_core::clipboard::Publication;
use fr_wire::{clipboard::session::synchronize::Received, negotiation::Offer};
use frd::{
    clipboard_quic::Error,
    native_clipboard::{Cleanup, Control, Phase},
    session_startup::NativeObserver,
};
use std::sync::{Mutex, MutexGuard};

/// Content-free evidence for the original clipboard. Configuration, native
/// setup, OS publication and actual native cleanup are separate observations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub requested: bool,
    pub configured: bool,
    /// Historical native setup/publication evidence, never current permission.
    pub native_started: bool,
    pub received_submitted: u64,
    pub received_unknown: u64,
    pub received_refused: u64,
    pub reason: Option<Error>,
    pub cleanup: Option<Cleanup>,
}

/// A completed lane refusal still proves the native owner was reaped. Explicit
/// native cleanup failure or an unwind cannot grant permission to replace it.
pub(super) fn cleanup_finished(result: Result<Cleanup, Error>) -> bool {
    matches!(result, Ok(Cleanup::NotStarted | Cleanup::Finished(Ok(()))))
        || matches!(result, Ok(Cleanup::Finished(Err(error)))
            if !matches!(error, Error::NativeCleanup | Error::Panicked))
}

/// The exact optional boundaries that attach the clipboard lane. A shared
/// bulk-record extension may remain useful to other negotiated lanes.
fn clipboard_capability(name: &str) -> bool {
    matches!(
        name,
        fr_wire::attachment::CLIPBOARD_CAPABILITY
            | fr_wire::clipboard::CAPABILITY
            | fr_wire::clipboard::startup::CAPABILITY
    )
}

impl Report {
    fn retain_reason(&mut self, reason: Error) {
        if matches!(self.reason, None | Some(Error::Closed)) {
            self.reason = Some(reason);
        }
    }

    fn receive(&mut self, received: Received) {
        match received {
            Received::Consumed(Some(receipt)) => {
                self.native_started = true;
                match receipt.publication {
                    Publication::SubmittedToOs => {
                        self.received_submitted = self.received_submitted.saturating_add(1);
                    }
                    Publication::UnknownEffect => {
                        self.received_unknown = self.received_unknown.saturating_add(1);
                    }
                    Publication::NotSubmitted(_) => {
                        self.received_refused = self.received_refused.saturating_add(1);
                    }
                }
            }
            Received::Refused(_) => {
                self.received_refused = self.received_refused.saturating_add(1);
            }
            Received::Deferred | Received::Consumed(None) => {}
        }
    }

    pub const fn cleanup_name(self) -> &'static str {
        match self.cleanup {
            None if self.configured => "unknown",
            None | Some(Cleanup::NotStarted) => "not_started",
            Some(Cleanup::Pending) => "pending",
            Some(Cleanup::Finished(Ok(()))) => "finished",
            Some(Cleanup::Finished(Err(_))) => "failed",
        }
    }
}

#[derive(Default)]
struct State {
    report: Report,
    /// Never replace this command's original configured owner on reconnect.
    spent: bool,
    source: Option<Control>,
}

impl State {
    fn collect(&mut self) {
        let Some(source) = &self.source else { return };
        // The native API has one ordinary slot and one final in-flight receipt.
        // Bound each callback even if a worker produces again while we collect.
        for _ in 0..2 {
            if let Some(received) = source.take_received() {
                self.report.receive(received);
            }
        }
        let status = source.status();
        self.report.native_started |= status.phase == Phase::Running;
        self.report.cleanup = Some(status.cleanup);
        if let Some(reason) = status.reason {
            self.report.retain_reason(reason);
        }
    }

    fn stop(&mut self) {
        self.collect();
        if let Some(source) = &self.source {
            source.stop();
        }
        self.collect();
    }
}

pub(super) struct Clipboard(Mutex<State>);

impl Clipboard {
    pub(super) fn new(requested: bool) -> Self {
        Self(Mutex::new(State {
            report: Report {
                requested,
                ..Report::default()
            },
            ..State::default()
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.spent = true;
            state.stop();
            state.report.retain_reason(Error::Poisoned);
            state
        })
    }

    fn available(&self) -> bool {
        let state = self.lock();
        state.report.requested && !state.spent
    }

    /// Preserve the original offer while this owner is available; later
    /// attempts remove only its optional clipboard markers. No new capability
    /// or previous consent is introduced into a replacement connection.
    pub(super) fn offer(&self, original: &Offer) -> Result<Offer, ()> {
        if self.available() {
            return Ok(original.clone());
        }
        if original
            .capabilities
            .iter()
            .any(|cap| cap.required && clipboard_capability(&cap.name))
        {
            return Err(());
        }
        let mut offer = original.clone();
        offer
            .capabilities
            .retain(|cap| !clipboard_capability(&cap.name));
        Ok(offer)
    }

    /// This registers a deferred native factory only. The existing observer
    /// opens it after an actual input grant and bilateral clipboard readiness.
    pub(super) fn configure(
        &self,
        observer: &mut NativeObserver,
        entropy: &Cx,
        live: impl FnOnce() -> bool,
    ) {
        let mut state = self.lock();
        if !state.report.requested || state.spent || !live() {
            return;
        }
        state.spent = true;
        match configuration(entropy).and_then(|config| observer.configure_clipboard(config)) {
            Ok(source) => {
                state.report.configured = true;
                state.source = Some(source);
            }
            // Clipboard is optional. A legacy/declining host or unavailable
            // native adapter must not remove ordinary control or direct Text.
            Err(error) => state.report.retain_reason(error),
        }
        state.collect();
    }

    pub(super) fn collect(&self) {
        self.lock().collect();
    }

    /// A local close/hide fences before any native UI or composition cleanup.
    /// The lock serializes an early stop against installation of the owner.
    pub(super) fn stop(&self) {
        let mut state = self.lock();
        state.spent = true;
        state.stop();
    }

    pub(super) fn hide(&self, was_visible: bool) {
        let mut state = self.lock();
        state.collect();
        // An initial not-yet-visible window has not revoked any positive
        // visibility. Keep its deferred default-on choice; an actual hide or
        // any already-started native lane is terminal even on repeated calls.
        if was_visible
            || state.report.native_started
            || matches!(
                state.report.cleanup,
                Some(Cleanup::Pending | Cleanup::Finished(_))
            )
        {
            state.spent = true;
            state.stop();
        }
    }

    /// An attempt that never configured a clipboard has no native owner to
    /// spend. Once configured, retirement is terminal across viewing retries.
    pub(super) fn retire(&self) {
        self.lock().stop();
    }

    pub(super) fn report(&self) -> Report {
        let mut state = self.lock();
        state.collect();
        state.report
    }

    pub(super) fn pending(&self) -> bool {
        self.report().cleanup == Some(Cleanup::Pending)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
fn configuration(entropy: &Cx) -> Result<frd::native_clipboard::Configuration, Error> {
    use fr_wire::clipboard::session::synchronize::IdentifierFailure;
    let entropy = entropy.clone();
    frd::native_clipboard::Configuration::new_with_guard_and_cleanup(
        true,
        std::time::Duration::from_secs(2),
        frd::windows_clipboard::factory(),
        move || {
            let mut bytes = [0; 16];
            entropy.random_bytes(&mut bytes);
            let id = u128::from_be_bytes(bytes);
            if id == 0 {
                Err(IdentifierFailure)
            } else {
                Ok(id)
            }
        },
    )
}

#[cfg(not(windows))]
fn configuration(_entropy: &Cx) -> Result<frd::native_clipboard::Configuration, Error> {
    // The iOS ABI and Mac test build never request this Windows adapter.
    Err(Error::NotNegotiated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_labels_never_confuse_configuration_with_native_completion() {
        let mut report = Report {
            requested: true,
            ..Report::default()
        };
        assert_eq!(report.cleanup_name(), "not_started");
        report.configured = true;
        assert_eq!(report.cleanup_name(), "unknown");
        report.cleanup = Some(Cleanup::Pending);
        assert_eq!(report.cleanup_name(), "pending");
        report.cleanup = Some(Cleanup::Finished(Err(Error::Native)));
        assert_eq!(report.cleanup_name(), "failed");
        report.cleanup = Some(Cleanup::Finished(Ok(())));
        assert_eq!(report.cleanup_name(), "finished");
        assert!(!report.native_started);
    }

    #[test]
    fn later_generic_close_cannot_erase_original_clipboard_refusal() {
        let mut report = Report::default();
        report.retain_reason(Error::NotNegotiated);
        report.retain_reason(Error::Closed);
        assert_eq!(report.reason, Some(Error::NotNegotiated));
        report.receive(Received::Deferred);
        report.receive(Received::Consumed(None));
        assert_eq!(report.received_submitted, 0);
        assert_eq!(report.received_unknown, 0);
        assert_eq!(report.received_refused, 0);
    }

    #[test]
    fn publication_counts_preserve_unknown_and_refused_effects_after_cleanup_failure() {
        use fr_core::clipboard::{Endpoint, PlatformError, Receipt, Stamp};
        let mut report = Report::default();
        let stamp = Stamp {
            id: 1,
            source: Endpoint::Host,
            sequence: 1,
        };
        for publication in [
            Publication::SubmittedToOs,
            Publication::UnknownEffect,
            Publication::NotSubmitted(PlatformError::Unavailable),
        ] {
            report.receive(Received::Consumed(Some(Receipt { stamp, publication })));
        }
        report.cleanup = Some(Cleanup::Finished(Err(Error::Native)));
        report.retain_reason(Error::Native);
        assert!(report.native_started);
        assert_eq!(report.received_submitted, 1);
        assert_eq!(report.received_unknown, 1);
        assert_eq!(report.received_refused, 1);
        assert_eq!(report.cleanup_name(), "failed");
        report.received_submitted = u64::MAX;
        report.receive(Received::Consumed(Some(Receipt {
            stamp,
            publication: Publication::SubmittedToOs,
        })));
        assert_eq!(report.received_submitted, u64::MAX);
    }

    #[test]
    fn unused_attempt_cleanup_does_not_spend_but_local_stop_always_does() {
        let clipboard = Clipboard::new(true);
        clipboard.retire();
        assert!(!clipboard.lock().spent);
        clipboard.stop();
        assert!(clipboard.lock().spent);
        assert!(!clipboard.report().configured);
        assert_eq!(clipboard.report().cleanup_name(), "not_started");
    }

    #[test]
    fn initial_invisibility_preserves_choice_but_actual_hide_and_started_owner_do_not() {
        let clipboard = Clipboard::new(true);
        clipboard.hide(false);
        assert!(!clipboard.lock().spent);
        clipboard.hide(true);
        assert!(clipboard.lock().spent);
        let running = Clipboard::new(true);
        running.lock().report.cleanup = Some(Cleanup::Pending);
        running.hide(false);
        assert!(running.lock().spent);
    }

    #[test]
    fn only_completed_native_cleanup_allows_attempt_replacement() {
        for result in [
            Ok(Cleanup::NotStarted),
            Ok(Cleanup::Finished(Ok(()))),
            Ok(Cleanup::Finished(Err(Error::Closed))),
            Ok(Cleanup::Finished(Err(Error::SwitchChanged))),
        ] {
            assert!(cleanup_finished(result));
        }
        for result in [
            Ok(Cleanup::Pending),
            Ok(Cleanup::Finished(Err(Error::NativeCleanup))),
            Ok(Cleanup::Finished(Err(Error::Panicked))),
            Err(Error::HandoffExpired),
            Err(Error::Cancelled),
        ] {
            assert!(!cleanup_finished(result));
        }
    }

    fn names(offer: &Offer) -> Vec<&str> {
        offer
            .capabilities
            .iter()
            .map(|cap| cap.name.as_str())
            .collect()
    }

    #[test]
    fn only_an_unspent_requested_clipboard_keeps_its_markers() {
        let original = super::super::control_policy::offer(true, true, false).unwrap();
        assert!(names(&original).into_iter().any(clipboard_capability));
        let fresh = Clipboard::new(true);
        assert!(fresh.available());
        assert_eq!(fresh.offer(&original).unwrap(), original);
        let retired = Clipboard::new(true);
        retired.stop();
        for clipboard in [Clipboard::new(false), retired] {
            assert!(!clipboard.available());
            let pruned = clipboard.offer(&original).unwrap();
            assert!(!names(&pruned).into_iter().any(clipboard_capability));
            let mut expected = original.clone();
            expected
                .capabilities
                .retain(|cap| !clipboard_capability(&cap.name));
            assert_eq!(pruned, expected);
        }
    }

    #[test]
    fn a_required_clipboard_marker_is_never_silently_dropped() {
        let mut original = super::super::control_policy::offer(true, true, false).unwrap();
        for cap in &mut original.capabilities {
            if clipboard_capability(&cap.name) {
                cap.required = true;
            }
        }
        assert_eq!(Clipboard::new(false).offer(&original), Err(()));
    }
}
