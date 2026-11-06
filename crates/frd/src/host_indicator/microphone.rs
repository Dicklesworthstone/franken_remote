//! Per-session, microphone-only revocation surface in the selected local
//! desktop. Readiness reports bounded native mapping/progress; it never grants
//! microphone permission or observation/input authority. Every loss invokes the
//! supplied original microphone permit's nonblocking, idempotent stop.
use super::process;
use crate::input_process::ProcessLaunch;
pub use process::{Control, Error, Status};

/// Retains the exact child until its native resources have been reaped. This
/// role has separate bounded capacity from the host observation indicator.
pub struct Owner(process::Owner);
impl Owner {
    pub fn start(
        launch: ProcessLaunch,
        on_stop: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        process::Owner::start_role(launch, on_stop, process::Role::Microphone).map(Self)
    }
    pub fn control(&self) -> Control {
        self.0.control()
    }
    pub fn stop(&self) {
        self.0.stop();
    }
    pub fn try_finish(&mut self) -> Option<Result<(), Error>> {
        self.0.try_finish()
    }
}
