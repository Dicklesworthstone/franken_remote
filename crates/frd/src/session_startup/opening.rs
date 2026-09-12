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
