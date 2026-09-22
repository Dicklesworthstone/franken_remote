//! Owned initial negotiation with one local consent notification. This has no
//! listener, consent UI, capture, input grant, or independently renewable proof.
use super::{Approval, Error, Host, HostSession, Role};
use std::{future::Future, time::Duration};

impl Host {
    /// Drive this already authenticated/admitted connection through local consent
    /// and `BindingAccepted`, then move the SAME owners into the running session.
    /// Notify a bounded local UI/queue once with the existing one-use capability
    /// and requested role. The callback must not block or infer consent from a
    /// remote packet. Returning successfully only records notification, not approval.
    ///
    /// No new timeout begins here: the Host's original deadline includes parked
    /// time, local approval, admission refresh and the final bound acknowledgement.
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
            let mut notified = false;
            loop {
                let host = state.host.as_mut().ok_or(Error::Closed)?;
                host.tick()?;
                if !notified && let Some(approval) = host.approval() {
                    (state.notify)(approval, host.role).map_err(|()| Error::Denied)?;
                    notified = true;
                    // A callback that crossed expiry or denied must not release
                    // SessionOpened merely because it returned successfully.
                    host.tick()?;
                }
                if host.is_complete() {
                    return state
                        .host
                        .take()
                        .ok_or(Error::Closed)?
                        .finish()?
                        .into_running();
                }
                host.drive(network_turn).await?;
            }
        })
    }
}
struct Opening<F> {
    host: Option<Host>,
    notify: F,
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
    pub(crate) fn retain_connection_check(
        &mut self,
        check: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(), Error> {
        self.transport
            .as_mut()
            .ok_or(Error::Closed)?
            .retain_lifetime_check(&self.cx, check)
            .map_err(Error::from)
    }
}
