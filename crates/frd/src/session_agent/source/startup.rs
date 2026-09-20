//! Atomic local registration before the original shared-display startup future.
use super::{Error, SessionAgent};
use crate::{
    media::{SharedCaptureUpdate, shared_publisher::Publisher},
    session_startup::{HostSession, PublisherError, SharedHost},
};
use fr_media::delivery::SendPolicy;
use std::{future::Future, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    Consent(Error),
    Publication(PublisherError),
}
impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for StartError {}

impl SessionAgent {
    /// Register the already locally approved selected source NOW, then open its
    /// first approved observer through normal display/attachment/decoder startup.
    /// Local permission, scope and unique source ownership are checked before a
    /// network future is returned. This does not grant initial observation or input.
    ///
    /// The future does NOT borrow this agent. Continue local permission events,
    /// source renewal and immediate revoke on the original event loop while it
    /// waits. Dropping the agent fences the source even while the future is held.
    /// A failed registration synchronously revokes only the joining session.
    ///
    /// Retrying an abandoned first-viewer attempt reuses only this agent's exact
    /// registration without changing cadence, budgets or the original child. The
    /// source's original unused-startup deadline still wins. Other agents cannot
    /// take it over. Late viewers use `HostSession::join_shared_display` rather than
    /// creating another source registration. Caller retains Publisher for reaping.
    pub fn start_shared_display<'a, E>(
        &mut self,
        session: HostSession,
        publisher: &'a mut Publisher,
        initial: &'a SharedCaptureUpdate,
        timeout: Duration,
        send: SendPolicy,
        entropy: E,
    ) -> impl Future<Output = Result<SharedHost, StartError>> + use<'a, E>
    where
        E: FnMut() -> Result<u128, ()> + 'a,
    {
        let registered = self.register_original_source(publisher);
        if registered.is_err() {
            session.original_observation().revoke();
        }
        // Construct the canonical guard at call time, including its absolute
        // budget and unpolled Drop. No caller code or native/network work here.
        let opening = session.start_shared_display(publisher, initial, timeout, send, entropy);
        async move {
            registered.map_err(StartError::Consent)?;
            opening.await.map_err(StartError::Publication)
        }
    }
    fn register_original_source(&mut self, publisher: &Publisher) -> Result<(), Error> {
        let original = self
            .sources
            .lock()
            .map_err(|_| Error::Poisoned)?
            .entries
            .iter()
            .flatten()
            .find(|entry| entry.owns(publisher))
            .cloned();
        // Never hold the registry lock over permission/scope checks. Reuse is
        // private to this local entry point; attach_shared_source stays strict.
        match original {
            Some(original) => original.recheck(self),
            None => self.attach_shared_source(publisher),
        }
    }
}
