//! The controlling viewer's shared text clipboard. On Windows it is frd's own
//! Win32 owner behind its visible "Text clipboard sharing is on." window with
//! Turn off (`frd::windows_clipboard`, the owner a Windows host uses). Other
//! platforms have no owner here yet and never request one.
//!
//! Only the initial controlling attempt opens it: a reconnect or reopen never
//! reacquires clipboard consent, and later attempts stop offering the lane.
//! No clipboard content enters this owner or its report.
#![forbid(unsafe_code)]

use fr_core::clipboard::Publication;
use fr_wire::{clipboard::session::synchronize::Received, negotiation::Offer};
use frd::{
    clipboard_quic::Error,
    native_clipboard::{Cleanup, Control, Phase},
    session_startup::NativeObserver,
};

/// Content-free evidence about the original clipboard. Retries cannot
/// overwrite it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Report {
    /// Asked for locally: `--control` without `--no-clipboard`.
    pub requested: bool,
    /// The host selected the lane and this viewer configured its owner.
    pub configured: bool,
    /// The native owner ran, or the peer's text arrived.
    pub active: bool,
    pub received_submitted: u64,
    pub received_unknown: u64,
    pub received_refused: u64,
    /// Sharing stopped locally: Turn off, or its window stopped answering.
    pub off: bool,
    pub reason: Option<Error>,
}

impl Report {
    fn retain(&mut self, reason: Error) {
        if matches!(self.reason, None | Some(Error::Closed)) {
            self.reason = Some(reason);
        }
    }
}

/// The optional capabilities that select the clipboard lane.
fn clipboard_capability(name: &str) -> bool {
    matches!(
        name,
        fr_wire::attachment::CLIPBOARD_CAPABILITY
            | fr_wire::clipboard::CAPABILITY
            | fr_wire::clipboard::startup::CAPABILITY
    )
}

#[derive(Default)]
pub(super) struct Clipboard {
    report: Report,
    spent: bool,
    source: Option<Control>,
}

impl Clipboard {
    pub(super) fn new(requested: bool) -> Self {
        Self {
            report: Report {
                requested,
                ..Report::default()
            },
            ..Self::default()
        }
    }

    /// Whether this attempt may still offer and open the clipboard.
    pub(super) const fn available(&self) -> bool {
        self.report.requested && !self.spent
    }

    pub(super) const fn report(&self) -> Report {
        self.report
    }

    /// This attempt's advertisement: the original while the clipboard is
    /// still available, otherwise the original without its optional
    /// clipboard markers. Nothing is ever added.
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

    /// Configure the owner on this attempt's observer before serving. Nothing
    /// opens now: frd runs the native factory on the clipboard worker only
    /// after the control grant and bilateral readiness. Spends the one use
    /// whether or not the host selected the lane.
    pub(super) fn configure(
        &mut self,
        observer: &mut NativeObserver,
        entropy: &asupersync::cx::Cx,
    ) {
        if !self.available() {
            return;
        }
        self.spent = true;
        match configure(observer, entropy) {
            Ok(source) => {
                self.report.configured = true;
                self.source = Some(source);
            }
            Err(error) => self.report.retain(error),
        }
    }

    /// Drain the UI's receipt slot and record status. Call every UI turn (a
    /// full slot defers the next receipt) and again after reaping.
    pub(super) fn collect(&mut self) {
        let Some(source) = &self.source else {
            return;
        };
        while let Some(received) = source.take_received() {
            match received {
                Received::Consumed(Some(receipt)) => {
                    self.report.active = true;
                    let count = match receipt.publication {
                        Publication::SubmittedToOs => &mut self.report.received_submitted,
                        Publication::UnknownEffect => &mut self.report.received_unknown,
                        Publication::NotSubmitted(_) => &mut self.report.received_refused,
                    };
                    *count = count.saturating_add(1);
                }
                Received::Refused(_) => {
                    self.report.received_refused = self.report.received_refused.saturating_add(1);
                }
                Received::Deferred | Received::Consumed(None) => {}
            }
        }
        let status = source.status();
        self.report.active |= status.phase == Phase::Running;
        self.report.off |= !status.enabled;
        if let Some(reason) = status.reason {
            self.report.retain(reason);
        }
    }

    /// Reap this attempt's owner (if any) within `deadline`. True when it
    /// never started or its native cleanup finished.
    pub(super) async fn reap(
        &mut self,
        observer: &mut NativeObserver,
        cx: &asupersync::cx::Cx,
        deadline: frd::worker::Deadline,
    ) -> bool {
        let reaped = observer.reap_clipboard(cx, deadline).await;
        for _ in 0..2 {
            let _ = observer.collect_clipboard();
            self.collect();
        }
        matches!(reaped, Ok(Cleanup::NotStarted | Cleanup::Finished(Ok(()))))
            || matches!(reaped, Ok(Cleanup::Finished(Err(error)))
                if !matches!(error, Error::NativeCleanup | Error::Panicked))
    }
}

#[cfg(windows)]
fn configure(
    observer: &mut NativeObserver,
    entropy: &asupersync::cx::Cx,
) -> Result<Control, Error> {
    let entropy = entropy.clone();
    let configuration = frd::native_clipboard::Configuration::new_with_guard_and_cleanup(
        true,
        std::time::Duration::from_secs(2),
        frd::windows_clipboard::factory(),
        move || {
            let mut bytes = [0; 16];
            entropy.random_bytes(&mut bytes);
            match u128::from_be_bytes(bytes) {
                0 => Err(fr_wire::clipboard::session::synchronize::IdentifierFailure),
                id => Ok(id),
            }
        },
    )?;
    observer.configure_clipboard(configuration)
}

#[cfg(not(windows))]
fn configure(_: &mut NativeObserver, _: &asupersync::cx::Cx) -> Result<Control, Error> {
    Err(Error::NativeSetup(
        fr_core::clipboard::PlatformError::Unavailable,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(offer: &Offer) -> Vec<&str> {
        offer
            .capabilities
            .iter()
            .map(|cap| cap.name.as_str())
            .collect()
    }

    #[test]
    fn only_an_unspent_requested_clipboard_keeps_its_markers() {
        let original = super::super::control_policy::offer(true, true).unwrap();
        assert!(names(&original).into_iter().any(clipboard_capability));
        let fresh = Clipboard::new(true);
        assert!(fresh.available());
        assert_eq!(fresh.offer(&original).unwrap(), original);
        for clipboard in [
            Clipboard::new(false),
            Clipboard {
                spent: true,
                ..Clipboard::new(true)
            },
        ] {
            assert!(!clipboard.available());
            let pruned = clipboard.offer(&original).unwrap();
            assert!(!names(&pruned).into_iter().any(clipboard_capability));
            // Everything else is the original, in order.
            let mut expected = original.clone();
            expected
                .capabilities
                .retain(|cap| !clipboard_capability(&cap.name));
            assert_eq!(pruned, expected);
        }
    }

    #[test]
    fn a_required_clipboard_marker_is_never_silently_dropped() {
        let mut original = super::super::control_policy::offer(true, true).unwrap();
        for cap in &mut original.capabilities {
            if clipboard_capability(&cap.name) {
                cap.required = true;
            }
        }
        assert_eq!(Clipboard::new(false).offer(&original), Err(()));
    }
}
