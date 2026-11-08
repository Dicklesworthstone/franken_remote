//! Fork/exec stays outside the authority poll, with the original launch custody.
use super::{
    Cx, Deadline, Error, Launch, ProtocolLimits, Time, Worker, bounded, live_deadline, now,
    runtime_ready,
};

impl Worker {
    pub(super) async fn spawn_checked<E: From<Error> + Send + 'static>(
        cx: &Cx,
        launch: Launch,
        limits: ProtocolLimits,
        deadline: Deadline,
        mut live: impl FnMut() -> Result<Time, E> + Clone + Send + 'static,
    ) -> Result<Self, E> {
        runtime_ready(cx)?;
        let mut previous = now(cx)?;
        live_deadline(cx, deadline, &mut previous, &mut live)?;
        if let Some(registration) = &launch.retirement {
            registration.launching();
        }
        let mut admitted = live.clone();
        let owner = cx.clone();
        let mut task = cx
            .spawn_blocking_drained(move |child| {
                let _ambient = child.clone().set_current_restricted();
                let mut check = || -> Result<(), E> {
                    child.checkpoint().map_err(|_| Error::Cancelled)?;
                    live_deadline(&owner, deadline, &mut previous, &mut admitted)?;
                    child.checkpoint().map_err(|_| Error::Cancelled.into())
                };
                check()?;
                let worker = Self::spawn(launch, limits)?;
                // Cancellation cannot interrupt fork/exec. A late result must
                // kill and transfer its child before it can leave this closure.
                check()?;
                Ok::<_, E>(worker)
            })
            .map_err(|_| Error::MissingRuntime)?;
        let worker = bounded(cx, deadline, live, async {
            task.join().await.map_err(|error| match error {
                asupersync::runtime::JoinError::Cancelled(_) => Error::Cancelled,
                _ => Error::Unavailable,
            })
        })
        .await??;
        // Until checked publication the launch, including a completed result
        // still held by the runtime, remains collectable by its cleanup owner.
        if let Some(registration) = &worker.retirement {
            registration.started();
        }
        Ok(worker)
    }
}
