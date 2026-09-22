//! First-observer approval through the original source's continuous service.
use super::{
    Catalog, Configuration, Entropy, Error, Fence, Host, LocalAction, Policy, Renewal,
    SessionAgent, Setup,
};
use crate::session_agent::source::desktop::{
    Report,
    launch::{Launch, Resources},
};
use crate::session_startup::{
    Approval,
    shared_viewers::{Admission, Ticket},
};
use fr_wire::{display::Select, negotiation::Role};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::Context,
    time::Duration,
};

impl SessionAgent {
    /// Create the selected native source only after first observation consent,
    /// then serve its original hub/capture owners without a caller-managed gap.
    /// Drive this on the independent OS-source task, not the first peer's scope.
    /// Its original Host deadline covers startup only; after startup, a departed
    /// first peer cannot cancel another admitted viewer's source or connection.
    ///
    /// The SAME bounded local-event callback and entropy supplier span approval,
    /// native preparation, first media attachment and continuous service. Actual
    /// OS permissions, protected/admitted Host and local source factory are still
    /// required; no listener, grant, permission or synthetic media is provided.
    ///
    /// announce receives weak admission access and the original first ticket
    /// once, BEFORE decoder completion. It is not a readiness/visibility signal.
    /// The service guard is already installed: reentrant joins are fenced on
    /// callback refusal, panic, revocation or abandonment, even with the failed
    /// future retained. Retain the launch Retirement outside this operation to
    /// confirm child exit; terminal effects are never automatically retried.
    #[allow(clippy::too_many_arguments)]
    pub fn run_native_shared_desktop<'a, F, S, N, L, A>(
        &'a mut self,
        mut first: Host,
        factory: F,
        select: S,
        policy: Policy,
        capture_interval: Duration,
        entropy: Entropy,
        notify: N,
        mut local: L,
        announce: A,
    ) -> Result<impl Future<Output = Result<Report, Error>> + Send + use<'a, F, S, N, L, A>, Error>
    where
        F: FnOnce() -> Result<Setup, ()> + Send + 'a,
        S: FnOnce(&Catalog) -> Result<(Select, Configuration), ()> + Send + 'a,
        N: FnMut(Approval, Role) -> Result<(), ()> + Send + 'a,
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
        A: FnOnce(Admission, Ticket) -> Result<(), ()> + Send + 'a,
    {
        let mut fence = Fence(Some(first.cancellation_context()));
        policy.validate().map_err(Error::Viewers)?;
        if capture_interval.is_zero() || capture_interval > Duration::from_secs(1) {
            return Err(Error::Capture(
                crate::media::shared_publisher::Error::InvalidBudget,
            ));
        }
        let (peer, binding, _) = first.shared_open_context().map_err(Error::Startup)?;
        let os = self.permissions().os_session_id();
        if binding.os_session.as_raw() != u128::from(os) {
            return Err(Error::Consent(super::ConsentError::SessionChanged));
        }
        Renewal::permission(self, os).map_err(Error::Consent)?;
        peer.timer_driver().ok_or(Error::Clock)?;
        let resources = Arc::new(Mutex::new(Resources::default()));
        let retained = resources.clone();
        let inner = async move {
            // Rechecking the same original Host does not restart its deadline.
            // Construct inside this body so one local callback spans both borrows.
            let mut desktop = self
                .open_native_shared_desktop(
                    first,
                    factory,
                    select,
                    policy,
                    entropy.clone(),
                    notify,
                    &mut local,
                )?
                .await?;
            let registration = self
                .register_original_source(&desktop.prepared.publisher)
                .map_err(Error::Consent)?;
            let admission = desktop.admissions();
            let ticket = desktop.first();
            *retained.lock().map_err(|_| Error::Closed)? = Resources {
                registration: Some(registration),
                cohort: Some(admission.clone()),
            };
            // Guard before caller code; no policy lock is held during announce.
            let (publisher, hub) = desktop.parts();
            let running =
                self.serve_shared_desktop(publisher, hub, capture_interval, entropy, local)?;
            announce(admission, ticket).map_err(|()| Error::LocalEvent)?;
            running.await
        };
        let operation = Launch::new(peer, resources, inner);
        fence.0 = None;
        Ok(operation)
    }
}
