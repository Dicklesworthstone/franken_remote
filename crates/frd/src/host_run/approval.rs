//! Device-attributed consent on the original host/share lifetime. The inbox
//! holds one original Approval, not peer identifiers or a reusable permission.
//! Local stop, selected-session loss and policy revision are checked by the
//! caller BEFORE every turn; native I/O and reaping stay in desktop_approval.
use super::{Error, LocalAction, Options};
use crate::{
    host_policy::{Approval as Mode, live},
    input_process::ProcessLaunch,
    session_startup::{Approval, desktop_approval::Prompt},
};
use asupersync::{cx::Cx, time::sleep};
use fr_wire::negotiation::Role;
use std::{path::{Path, PathBuf}, sync::{Arc, Mutex}, time::Duration};

/// One original pre-observation budget, including negotiation and native
/// startup. No packet, UI progress or positive decision refreshes this time.
pub const STARTUP_BUDGET: Duration = Duration::from_secs(30);
pub(super) fn require(request: &mut crate::native_connection::host::Request) {
    request.session.require_approval = true;
    request.session.startup_timeout = STARTUP_BUDGET;
}

/// Locally installed session-UI image. Supplying it enables neither approval
/// nor sharing; the current saved/explicit policy still selects the mode.
#[derive(Debug, Clone)]
pub struct Configuration {
    image: PathBuf,
}
impl Configuration {
    pub fn new(image: &Path) -> Result<Self, Error> {
        if !image.is_absolute() { return Err(Error::Configuration); }
        Ok(Self { image: image.to_owned() })
    }
    pub(super) fn validate(&self, options: &Options) -> Result<(), Error> {
        // Do not approve a desktop whose lock/logout/switch boundary is absent
        // or monitors another selected display/user. No ambient session guess.
        let monitor = options.session_monitor.as_ref().ok_or(Error::LocalApprovalUnavailable)?;
        if monitor.selection.display != options.display
            || monitor.selection.uid != rustix::process::geteuid().as_raw()
            || monitor.selection.validate().is_err()
        {
            return Err(Error::LocalApprovalUnavailable);
        }
        ProcessLaunch::new(&self.image, &options.display, options.xauthority.as_deref(), 1)
            .map(|_| ()).map_err(|_| Error::Configuration)
    }
}

pub(super) fn required(epoch: Option<&live::Lease>) -> Result<bool, Error> {
    epoch.map_or(Ok(false), |epoch| {
        epoch.check().map(|policy| policy.approval_mode == Mode::Local).map_err(Error::Policy)
    })
}
struct Inbox {
    pending: Option<Approval>,
    busy: bool,
    closed: bool,
}
impl Inbox {
    fn close(&mut self) {
        self.closed = true;
        if let Some(original) = self.pending.take() { let _ = original.decide(false); }
    }
}
#[derive(Clone)]
pub(super) struct Notify {
    inbox: Arc<Mutex<Inbox>>,
    enabled: bool,
}
impl Notify {
    pub(super) fn submit(&self, original: Approval, role: Role) -> Result<(), ()> {
        let result = (|| {
            if !self.enabled || original.role() != role { return Err(()); }
            original.check_pending().map_err(|_| ())?;
            let mut inbox = self.inbox.lock().map_err(|_| ())?;
            if inbox.closed || inbox.busy { return Err(()); }
            inbox.pending = Some(original.clone());
            inbox.busy = true;
            Ok(())
        })();
        if result.is_err() { let _ = original.decide(false); }
        result
    }
}

/// One owner per original OS share. Notifications cannot launch replacement
/// work, choose a UI path, consume an answer or outlive this owner's close.
pub(super) struct Owner {
    notify: Notify,
    configuration: Option<Configuration>,
    display: String,
    xauthority: Option<PathBuf>,
    prompt: Option<Prompt>,
}
impl Owner {
    pub(super) fn new(configuration: Option<&Configuration>, options: &Options) -> Self {
        Self {
            notify: Notify {
                inbox: Arc::new(Mutex::new(Inbox { pending: None, busy: false, closed: false })),
                enabled: configuration.is_some(),
            },
            configuration: configuration.cloned(),
            display: options.display.clone(),
            xauthority: options.xauthority.clone(),
            prompt: None,
        }
    }
    pub(super) fn notify(&self) -> Notify { self.notify.clone() }
    pub(super) fn enabled(&self) -> bool { self.notify.enabled }
    pub(super) fn turn(&mut self, action: LocalAction) -> LocalAction {
        if action == LocalAction::Stop {
            self.close();
            return LocalAction::Stop;
        }
        let incoming = {
            let locked = self.notify.inbox.lock();
            match locked {
                Ok(mut inbox) if !inbox.closed => Ok(inbox.pending.take()),
                _ => Err(()),
            }
        };
        let original = match incoming {
            Ok(original) => original,
            Err(()) => { self.close(); return LocalAction::Stop; }
        };
        if let Some(original) = original {
            let launch = self.configuration.as_ref().ok_or(())
                .and_then(|config| super::random_nonzero_u128().map_err(|_| ())
                    .and_then(|epoch| ProcessLaunch::new(&config.image, &self.display,
                        self.xauthority.as_deref(), epoch).map_err(|_| ())));
            self.prompt = match launch {
                Ok(launch) => Prompt::start(original.clone(), launch).ok(),
                Err(()) => None,
            };
            if self.prompt.is_none() {
                let _ = original.decide(false);
                self.release_inbox();
            }
        }
        if let Some(prompt) = &mut self.prompt {
            match prompt.take_decision() {
                Ok(None) => return action,
                Ok(Some(_)) => {} // The original capability, not this inbox, accepted the answer.
                Err(_) => {
                    prompt.cancel();
                    match prompt.try_finish() {
                        None => return action,
                        Some(Ok(())) => {}
                        Some(Err(_)) => { self.close(); return LocalAction::Stop; }
                    }
                }
            }
            self.prompt = None;
            self.release_inbox();
        }
        action
    }
    fn release_inbox(&self) {
        if let Ok(mut inbox) = self.notify.inbox.lock() { inbox.busy = false; }
    }
    pub(super) fn close(&self) {
        self.notify.inbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).close();
        if let Some(prompt) = &self.prompt { prompt.cancel(); }
    }
    /// Original native cleanup before the host can create another share. A
    /// pending reap is retained by Prompt::Drop and keeps the global UI slot.
    pub(super) async fn finish(&mut self, cx: &Cx) -> Result<(), Error> {
        self.close();
        let Some(prompt) = &mut self.prompt else { return Ok(()); };
        let until = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match prompt.try_finish() {
                Some(Ok(())) => { self.prompt = None; return Ok(()); }
                Some(Err(_)) => return Err(Error::Cleanup("approval")),
                None if std::time::Instant::now() < until && !cx.is_cancel_requested() => {}
                None => return Err(Error::Cleanup("approval")),
            }
            sleep(cx.now(), Duration::from_millis(5)).await;
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) { self.close(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::HostBootId;
    use fr_tailnet::Scope;

    fn options() -> Options {
        Options {
            socket: None, port: 8443, interface: "tailscale0".into(),
            worker: PathBuf::from("/unprovisioned/fr-media-worker"), display: ":0".into(),
            xauthority: None, trust_roots: PathBuf::from("/unprovisioned/roots"),
            sharing: Scope::OwnUser, fps: 30, bitrate: 8_000_000,
            ingress_tools: None, once: true, handle_signals: false,
            input_agent: None, clipboard: false, audio: None, files: None,
            session_monitor: None,
        }
    }
    #[test]
    fn local_consent_requires_the_same_monitored_desktop_and_user() {
        use crate::session_monitor::{Configuration as Monitor, Selection};
        let ui = Configuration::new(Path::new("/unprovisioned/fr-observation-indicator")).unwrap();
        let mut options = options();
        assert_eq!(ui.validate(&options), Err(Error::LocalApprovalUnavailable));
        options.session_monitor = Some(Monitor {
            image: PathBuf::from("/unprovisioned/fr-session-monitor"),
            selection: Selection {
                session: "c2".into(), uid: rustix::process::geteuid().as_raw(),
                seat: "seat0".into(), display: options.display.clone(),
            },
        });
        assert_eq!(ui.validate(&options), Ok(()));
        options.session_monitor.as_mut().unwrap().selection.display = ":1".into();
        assert_eq!(ui.validate(&options), Err(Error::LocalApprovalUnavailable));
        options.session_monitor.as_mut().unwrap().selection.display = ":0".into();
        options.session_monitor.as_mut().unwrap().selection.uid ^= 1;
        assert_eq!(ui.validate(&options), Err(Error::LocalApprovalUnavailable));
        assert!(Configuration::new(Path::new("relative-ui")).is_err());
    }
    #[test]
    fn requiring_consent_changes_neither_peer_scope_nor_live_input_budgets() {
        let mut request = super::super::request(1, HostBootId::from_raw(9), 5,
            Scope::OwnUser, (Some(super::super::control_capabilities()), true, true), true).unwrap();
        let before = request.session.clone();
        assert!(!before.require_approval);
        assert_eq!(before.startup_timeout, Duration::from_secs(5));
        require(&mut request);
        assert!(request.session.require_approval);
        assert_eq!(request.session.startup_timeout, STARTUP_BUDGET);
        assert_eq!(request.session.offer, before.offer);
        assert_eq!(request.session.binding, before.binding);
        assert_eq!(request.admission.scope, Scope::OwnUser);
        assert_eq!(request.session.authority.authorization_lifetime, before.authority.authorization_lifetime);
        assert_eq!(request.session.authority.ticket_lifetime, before.authority.ticket_lifetime);
        assert_eq!(request.session.transport.record_lifetime_micros, before.transport.record_lifetime_micros);
    }
    #[test]
    fn closing_a_share_fences_escaped_notifications_and_never_deadlocks_a_turn() {
        let mut owner = Owner::new(None, &options());
        let notification = owner.notify();
        assert!(!owner.enabled());
        owner.close();
        assert!(notification.inbox.lock().unwrap().closed);
        assert_eq!(owner.turn(LocalAction::Continue), LocalAction::Stop);
        assert_eq!(owner.turn(LocalAction::Stop), LocalAction::Stop);
        drop(owner);
        assert!(notification.inbox.lock().unwrap().closed);
    }
}
