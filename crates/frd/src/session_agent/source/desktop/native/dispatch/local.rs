//! Original source-lifetime and dispatcher checks around the local callback.
//! One content-free failure is retained; no callback executes after that fence.
use super::{Cx, LocalAction, Route, SessionAgent, Shared};
use crate::session_agent::source::desktop::{Error, LocalFailure};
use std::{sync::Arc, task::Context};

pub(super) struct Events<L> {
    lifetime: Cx,
    shared: Arc<Shared>,
    events: L,
    failure: Option<LocalFailure>,
}
impl<L> Events<L> {
    pub(super) fn new(lifetime: Cx, shared: Arc<Shared>, events: L) -> Self {
        Self {
            lifetime,
            shared,
            events,
            failure: None,
        }
    }

    fn check(&self) -> Result<(), LocalFailure> {
        self.lifetime
            .checkpoint()
            .map_err(|_| LocalFailure::SourceLifetimeCancelled)?;
        if matches!(
            *self
                .shared
                .route
                .lock()
                .map_err(|_| LocalFailure::DispatcherPoisoned)?,
            Route::Closed
        ) {
            return Err(LocalFailure::DispatcherClosed);
        }
        Ok(())
    }

    /// Older startup/shared owners expose a unit-error callback. Replace only
    /// their exact LocalEvent marker, never a native cleanup or other failure.
    pub(super) fn annotate(&self, error: Error) -> Error {
        match (error, self.failure) {
            (Error::LocalEvent, Some(cause)) => Error::LocalFailure {
                cause,
                session: None,
            },
            _ => error,
        }
    }
}
impl<L> Events<L>
where
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()>,
{
    pub(super) fn turn(
        &mut self,
        agent: &mut SessionAgent,
        task: &mut Context<'_>,
    ) -> Result<LocalAction, LocalFailure> {
        if let Some(cause) = self.failure {
            return Err(cause);
        }
        let result = self.check().and_then(|()| {
            let action = (self.events)(agent, task).map_err(|()| LocalFailure::CallbackFailed)?;
            self.check()?;
            Ok(action)
        });
        if let Err(cause) = result {
            self.failure = Some(cause);
        }
        result
    }
}

#[cfg(test)]
mod tests;
