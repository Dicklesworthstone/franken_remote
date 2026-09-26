//! Owned initial negotiation with one local consent notification. This has no
//! listener, consent UI, capture, input grant, or independently renewable proof.
use super::{Approval, Error, Host, HostSession, Role};
use std::{future::Future, time::Duration};

mod refusal;

impl Host {
    /// Drive this already authenticated/admitted connection through local consent
    /// and `BindingAccepted`, then move the SAME owners into the running session.
    /// Notify a bounded local UI/queue once with the existing one-use capability
    /// and requested role. The callback must not block or infer consent from a
    /// remote packet. Returning successfully only records notification, not approval.
    ///
    /// No new negotiation timeout begins here: the original deadline includes parked
    /// time, local approval, admission refresh and the final bound acknowledgement.
    /// A failure may drain one bounded refusal only AFTER authority is fenced.
    /// That reporting budget never extends negotiation or observation authority.
    /// Dropping even an unpolled open retires consent and revokes the peer before
    /// dropping callback state. Success transfers ownership without restarting it.
    pub fn open<'a>(
        self,
        network_turn: Duration,
        notify: impl FnMut(Approval, Role) -> Result<(), ()> + 'a,
    ) -> impl Future<Output = Result<HostSession, Error>> + 'a {
        let state = Opening {
            host: Some(self),
            notify,
        };
        Box::pin(async move {
            let mut state = state;
            if network_turn < Duration::from_millis(1) || network_turn > Duration::from_millis(100)
            {
                return Err(Error::InvalidConfiguration);
            }
            if let Err(error) = state.negotiate(network_turn).await {
                if let Some(host) = &mut state.host {
                    refusal::report(host, error).await;
                }
                return Err(error);
            }
            state
                .host
                .take()
                .ok_or(Error::Closed)?
                .finish()?
                .into_running()
        })
    }
}
struct Opening<F> {
    host: Option<Host>,
    notify: F,
}
impl<F: FnMut(Approval, Role) -> Result<(), ()>> Opening<F> {
    async fn negotiate(&mut self, network_turn: Duration) -> Result<(), Error> {
        let mut notified = false;
        loop {
            let host = self.host.as_mut().ok_or(Error::Closed)?;
            // Opening owns the same cancellation/drop guard as the public
            // tick/drive wrappers. Keep transport only long enough to report a
            // terminal pre-observation refusal; never retry failed negotiation.
            host.step()?;
            if !notified && let Some(approval) = host.approval() {
                (self.notify)(approval, host.role).map_err(|()| Error::Denied)?;
                notified = true;
                // Returning from a callback is not permission to cross expiry
                // or denial, even if it first recorded an approval.
                host.step()?;
            }
            if host.is_complete() {
                return Ok(());
            }
            host.drive_inner(network_turn).await?;
        }
    }
}
impl<F> Drop for Opening<F> {
    fn drop(&mut self) {
        if let Some(host) = &mut self.host {
            host.close();
        }
    }
}

impl Host {
    // Reuse the same post-TLS owner for both the first and later shared viewers.
    // This is NOT listener admission. Never restart a partially driven Host.
    pub(crate) fn bind_shared_source(
        &mut self,
        source: crate::media::shared_publisher::JoinQueue,
    ) -> Result<(super::Cx, super::ControlBinding, u64), Error> {
        let original = self.restrict_shared_observer()?;
        source.check_source().map_err(Error::SharedPublication)?;
        self.shared_source = Some(source);
        Ok(original)
    }
    pub(crate) fn cancellation_context(&self) -> super::Cx {
        self.cx.clone()
    }
    // First-source admission has no Publisher/JoinQueue yet. Restrict the role
    // before ClientHello without fabricating source permission or a source owner.
    pub(crate) fn restrict_shared_observer(
        &mut self,
    ) -> Result<(super::Cx, super::ControlBinding, u64), Error> {
        let original = self.shared_open_context()?;
        self.observation_only = true;
        Ok(original)
    }
    pub(crate) fn shared_open_context(
        &mut self,
    ) -> Result<(super::Cx, super::ControlBinding, u64), Error> {
        if self.phase != super::Phase::Hello || self.len != 0 || self.shared_source.is_some() {
            return Err(Error::Order);
        }
        self.check()?;
        Ok((self.cx.clone(), self.config.binding, self.until))
    }
}

impl super::Configuration {
    pub(crate) fn check_incoming(&self) -> Result<(), Error> {
        self.validate()?;
        self.transport.validate()?;
        // The native listener advertises these windows before Host exists.
        // A later adapter cannot retract already-advertised transport credit.
        if self.transport.stream_window != 65_536 || self.transport.connection_window != 524_288 {
            return Err(Error::InvalidConfiguration);
        }
        Ok(())
    }
}
impl Host {
    pub(crate) fn retain_connection_checks(
        &mut self,
        check: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
        terminal: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(), Error> {
        self.transport
            .as_mut()
            .ok_or(Error::Closed)?
            .retain_lifetime_checks(&self.cx, check, terminal)
            .map_err(Error::from)
    }
}

impl HostSession {
    // Fail before a native factory, monitor discovery or capture is invoked.
    // Successful observation negotiation alone does not admit a media profile.
    pub(crate) fn require_shared_profile(&mut self) -> Result<(), Error> {
        self.check()?;
        if !super::native_control::profile(self.selection(), false) {
            return Err(Error::InvalidConfiguration);
        }
        Ok(())
    }
    // The exclusive controlled share requires the complete explicit control
    // profile (input attachment, grant, clock, presentation proof) before any
    // native factory, discovery or capture. Negotiating the role is not a grant.
    // The host offers those capabilities optionally, so a controller that did
    // not select them is the PEER's missing requirement, not a host fault.
    pub(crate) fn require_controlled_profile(&mut self) -> Result<(), Error> {
        self.check()?;
        if !super::native_control::profile(self.selection(), true) {
            return Err(Error::Protocol(
                fr_wire::negotiation::Error::RequiredCapability,
            ));
        }
        Ok(())
    }
}
