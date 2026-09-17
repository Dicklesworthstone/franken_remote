//! Original-session ownership of a local, revocation-only native surface.
//! Platform implementations supply mapping/liveness, not approval or authority.
use crate::{media::ObservationControl, session_startup::HostSession, worker};
use asupersync::cx::Cx;
use std::{fmt, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Opening,
    /// The platform's controls are installed and rendering was submitted. This
    /// is not evidence of physical/compositor visibility or human consent.
    Ready,
    Stopped,
}
/// All methods MUST be nonblocking and must not invoke arbitrary application
/// callbacks. Native calls belong on the platform's separately owned thread.
/// A surface only revokes its original observation; it never creates permission.
pub trait Surface: Send + Sync {
    fn original(&self) -> &ObservationControl;
    fn state(&self) -> State;
    fn stop(&self);
    /// Idempotent. True proves the original native owner has finished cleanup,
    /// not merely that its admission flag is stopped.
    fn finish(&mut self) -> bool;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    AlreadyAttached,
    WrongOwner,
    Closed,
}
/// A refused attachment returns its original owner intact. In particular,
/// refusing a foreign surface does not stop that foreign session as a side effect.
pub struct Rejected {
    pub reason: AttachError,
    pub surface: Box<dyn Surface>,
}
impl fmt::Debug for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RejectedSharingSurface")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl HostSession {
    /// Transfer one ORIGINAL local surface before native publication. The host
    /// retains it across media bootstrap, control promotion and continuous service.
    /// Registration creates no grant and does not reset either startup deadline.
    /// Opening surfaces allow metadata/renewal but block native publisher launch.
    pub fn attach_sharing_surface(&mut self, surface: Box<dyn Surface>) -> Result<(), Rejected> {
        let reason = if self.sharing_surface.is_some() {
            Some(AttachError::AlreadyAttached)
        } else if !surface.original().same_owner(&self.original_observation()) {
            Some(AttachError::WrongOwner)
        } else if self.check().is_err() || surface.state() == State::Stopped {
            Some(AttachError::Closed)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(Rejected { reason, surface });
        }
        self.sharing_surface = Some(surface);
        Ok(())
    }
    pub(crate) fn sharing_surface_ready(&mut self) -> Result<bool, crate::session_startup::Error> {
        self.check()?;
        Ok(self
            .sharing_surface
            .as_ref()
            .is_none_or(|surface| surface.state() == State::Ready))
    }
    /// Close the original session first, then collect native UI completion using
    /// a separate cleanup context and the caller's ORIGINAL absolute deadline.
    /// A timeout leaves the original surface retained for a subsequent reap.
    /// This neither releases held keys nor reaps media/clipboard workers.
    pub async fn reap_sharing_surface(
        &mut self,
        cleanup: &Cx,
        deadline: worker::Deadline,
    ) -> Result<(), worker::Error> {
        self.close();
        loop {
            if self.sharing_surface.as_mut().is_none_or(|s| s.finish()) {
                return Ok(());
            }
            cleanup.checkpoint().map_err(|_| worker::Error::Cancelled)?;
            let now = cleanup
                .timer_driver()
                .ok_or(worker::Error::MissingRuntime)?
                .now();
            if now >= deadline.time() {
                return Err(worker::Error::ReapPending);
            }
            asupersync::time::sleep(now, Duration::from_millis(1)).await;
        }
    }
}
