//! One controlling attempt's native clipboard, original privacy surface and
//! retained cleanup. Clipboard content never enters this main-thread owner.
#![forbid(unsafe_code)]

use super::Window;
use crate::macos_clipboard_privacy::Heartbeat;
use asupersync::cx::Cx;
use fr_core::{
    clipboard::{PlatformError, Publication},
    limits::ProtocolLimits,
};
use fr_wire::clipboard::session::synchronize::{IdentifierFailure, Received};
use frd::{
    clipboard_quic::Error,
    native_clipboard::{Cleanup, Configuration, Control, Phase},
    session_startup::NativeObserver,
};
use std::{
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const WATCH_TURN: Duration = Duration::from_millis(10);

/// Bound to the toolbar before startup can open a clipboard. An early Off is
/// retained even before the one original application Control exists.
#[derive(Clone, Default)]
pub(crate) struct Privacy {
    heartbeat: Heartbeat,
    source: Arc<Mutex<Option<Control>>>,
}
impl Privacy {
    pub(crate) fn stop(&self, user_off: bool) {
        self.heartbeat.stop(user_off);
        let source = self
            .source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(source) = source.as_ref() {
            source.stop();
        }
    }
    fn bind(&self, source: Control) -> Result<(), Error> {
        let mut original = self.source.lock().map_err(|_| Error::Poisoned)?;
        if original.is_some() {
            return Err(Error::AlreadyAttached);
        }
        if self.heartbeat.stopped() {
            source.stop();
        }
        *original = Some(source);
        Ok(())
    }
}

// Every watchdog exit fences the same owner, including an unexpected unwind.
// It never claims that the separately retained native worker has been reaped.
struct WatchdogFence(Privacy);
impl Drop for WatchdogFence {
    fn drop(&mut self) {
        self.0.stop(false);
    }
}

/// Evidence for the original clipboard only; retries cannot overwrite it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Report {
    pub requested: bool,
    pub configured: bool,
    pub active: bool,
    pub received_submitted: u64,
    pub received_unknown: u64,
    pub received_refused: u64,
    pub disabled: bool,
    pub reason: Option<Error>,
    pub cleanup: Option<Cleanup>,
}
impl Report {
    fn retain_reason(&mut self, reason: Error) {
        if matches!(self.reason, None | Some(Error::Closed)) {
            self.reason = Some(reason);
        }
    }
}

#[derive(Default)]
pub(crate) struct Clipboard {
    report: Report,
    spent: bool,
    bound: bool,
    source: Option<Control>,
    privacy: Privacy,
    watchdog: Option<JoinHandle<()>>,
    displayed: Option<u32>,
}

impl Clipboard {
    pub(crate) fn request(&mut self, requested: bool) {
        if !self.spent {
            self.report.requested = requested;
        }
    }

    pub(crate) fn available(&self) -> bool {
        self.report.requested && !self.spent && !self.privacy.heartbeat.stopped()
    }

    pub(crate) const fn report(&self) -> Report {
        self.report
    }

    pub(crate) fn install(&mut self, window: &Window) -> Result<(), crate::NativeError> {
        if !self.available() {
            return Ok(());
        }
        window.clipboard_bind(self.privacy.clone())?;
        self.bound = true;
        Ok(())
    }

    pub(crate) fn configure(
        &mut self,
        observer: &mut NativeObserver,
        window: Option<&Window>,
        entropy: Option<Cx>,
    ) {
        if !self.available() {
            return;
        }
        self.spent = true;
        let result = self.configure_inner(observer, window, entropy);
        if let Err(error) = result {
            self.report.retain_reason(error);
            self.stop(window);
            if let Some(window) = window {
                let _ = window.clipboard_update(3);
            }
        }
        self.collect();
    }

    fn configure_inner(
        &mut self,
        observer: &mut NativeObserver,
        window: Option<&Window>,
        entropy: Option<Cx>,
    ) -> Result<(), Error> {
        let window = window.ok_or(Error::NativeSetup(PlatformError::Unavailable))?;
        let entropy = entropy.ok_or(Error::NativeSetup(PlatformError::Unavailable))?;
        let privacy = self.privacy.clone();
        let configuration = Configuration::new_with_guard(
            true,
            Duration::from_secs(2),
            move |source, access| {
                crate::macos_clipboard::MacClipboard::open(
                    &ProtocolLimits::ABSOLUTE,
                    move |until| {
                        if !privacy.heartbeat.live(Instant::now()) {
                            source.stop();
                            return false;
                        }
                        // A temporary view-recovery pause is not a local Off.
                        source.status().enabled && access.check(until)
                    },
                )
            },
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
        )?;
        let source = observer.configure_clipboard(configuration)?;
        self.report.configured = true;
        self.source = Some(source.clone());
        if !self.bound {
            return Err(Error::NativeSetup(PlatformError::Unavailable));
        }
        self.privacy.bind(source.clone())?;
        self.turn(Some(window));
        if !source.status().enabled {
            return Ok(());
        }
        let privacy = self.privacy.clone();
        self.watchdog = Some(
            thread::Builder::new()
                .name("fr-mac-clipboard-privacy".into())
                .spawn(move || {
                    let _fence = WatchdogFence(privacy.clone());
                    loop {
                        if !source.status().enabled {
                            return;
                        }
                        if !privacy.heartbeat.live(Instant::now()) {
                            source.stop();
                            return;
                        }
                        thread::sleep(WATCH_TURN);
                    }
                })
                .map_err(|_| Error::NativeSetup(PlatformError::Unavailable))?,
        );
        Ok(())
    }

    pub(crate) fn turn(&mut self, window: Option<&Window>) {
        let Some(source) = &self.source else {
            return;
        };
        if source.status().enabled {
            let visible = window
                .and_then(|window| window.clipboard_visible().ok())
                .unwrap_or(false);
            if !visible || !self.privacy.heartbeat.progress(Instant::now()) {
                self.privacy.stop(false);
                source.stop();
            }
        }
        self.collect();
        let status = self.source.as_ref().map(Control::status);
        let display = match status {
            Some(status) if self.report.disabled || !status.enabled => 2,
            Some(status) if status.phase == Phase::Running => 1,
            Some(status) if matches!(status.phase, Phase::Closed | Phase::Retired) => 4,
            _ if self.report.reason.is_some() => 3,
            _ => 0,
        };
        if self.displayed != Some(display)
            && let Some(window) = window
        {
            if window.clipboard_update(display).is_err() {
                self.privacy.stop(false);
                if let Some(source) = &self.source {
                    source.stop();
                }
            }
            self.displayed = Some(display);
        }
    }

    pub(crate) fn collect(&mut self) {
        self.report.disabled |= self.privacy.heartbeat.user_off();
        let Some(source) = &self.source else {
            return;
        };
        while let Some(received) = source.take_received() {
            match received {
                Received::Consumed(Some(receipt)) => {
                    self.report.active = true;
                    match receipt.publication {
                        Publication::SubmittedToOs => {
                            self.report.received_submitted =
                                self.report.received_submitted.saturating_add(1)
                        }
                        Publication::UnknownEffect => {
                            self.report.received_unknown =
                                self.report.received_unknown.saturating_add(1)
                        }
                        Publication::NotSubmitted(_) => {
                            self.report.received_refused =
                                self.report.received_refused.saturating_add(1)
                        }
                    }
                }
                Received::Refused(_) => {
                    self.report.received_refused = self.report.received_refused.saturating_add(1)
                }
                Received::Deferred | Received::Consumed(None) => {}
            }
        }
        let status = source.status();
        self.report.active |= status.phase == Phase::Running;
        self.report.cleanup = Some(status.cleanup);
        if let Some(reason) = status.reason {
            self.report.retain_reason(reason);
        }
    }

    pub(crate) fn stop(&mut self, window: Option<&Window>) {
        if !self.bound && !self.spent {
            return;
        }
        self.spent = true;
        self.privacy.stop(false);
        if let Some(source) = &self.source {
            source.stop();
        }
        self.collect();
        if self.report.configured
            && let Some(window) = window
        {
            let _ = window.clipboard_update(2);
        }
    }

    pub(crate) fn finished(&mut self) -> bool {
        if self
            .watchdog
            .as_ref()
            .is_some_and(|watchdog| !watchdog.is_finished())
        {
            return false;
        }
        if let Some(watchdog) = self.watchdog.take()
            && watchdog.join().is_err()
        {
            self.report
                .retain_reason(Error::NativeSetup(PlatformError::Unavailable));
        }
        true
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.stop(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_before_native_start_does_not_spend_an_unused_choice() {
        let mut clipboard = Clipboard::default();
        clipboard.request(true);
        assert!(clipboard.available());
        clipboard.stop(None);
        assert!(clipboard.available());
        assert!(!clipboard.report().disabled);
    }

    #[test]
    fn an_early_off_and_a_bound_attempt_cannot_be_reoffered() {
        let mut clipboard = Clipboard::default();
        clipboard.request(true);
        // Lifecycle policy only: this fixture creates no native window/worker.
        clipboard.bound = true;
        clipboard.privacy.stop(true);
        clipboard.collect();
        assert!(!clipboard.available());
        assert!(clipboard.report().disabled);
        clipboard.stop(None);
        clipboard.request(true);
        assert!(!clipboard.available());
        assert!(clipboard.report().disabled);
    }

    #[test]
    fn normal_teardown_cannot_hide_the_first_meaningful_reason() {
        let mut report = Report::default();
        report.retain_reason(Error::Closed);
        report.retain_reason(Error::NativeSetup(PlatformError::Unavailable));
        report.retain_reason(Error::Closed);
        report.retain_reason(Error::Poisoned);
        assert_eq!(
            report.reason,
            Some(Error::NativeSetup(PlatformError::Unavailable))
        );
    }
}
