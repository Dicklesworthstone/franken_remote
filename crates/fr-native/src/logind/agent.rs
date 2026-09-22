//! Feed real negative logind evidence to the ORIGINAL local `SessionAgent`.
//! Never synthesize approval, capture permission, an unlock, or another session.
use super::{Control, Status, StopReason};
use asupersync::{cx::Cx, types::CancelKind};
use fr_core::{input_submission::Operation, time::HostInstant};
use frd::{
    input_watchdog,
    session_agent::{
        AgentIdentity, ImmediateRevokeOutcome, PlatformKind, SessionAgent,
        source::desktop::LocalAction,
    },
};
use std::task::Context;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Opening,
    Evidence(StopReason),
    WrongAgent,
    SessionChanged,
    UnsupportedPlatform,
    AlreadyRevoked,
    SessionLocked,
    Cancelled,
    Clock,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session-events: {self:?}")
    }
}
impl std::error::Error for Error {}

/// These are pending RELEASE operations, not evidence of their OS submission.
/// Keep this receipt even after the service fails. Submit cleanup on the original
/// input owner, preserving unknown effects; mark `outcome.os_cleanup` only after
/// that owner confirms completion. Revocation alone is never that confirmation.
#[must_use = "retain pending input releases and independently observe native cleanup"]
#[derive(Debug)]
pub struct Cleanup {
    pub cause: Error,
    pub outcome: ImmediateRevokeOutcome,
    pub releases: Vec<Operation>,
}

/// Bind only after fresh logind evidence, on the local selected OS-share owner.
/// The supplied Cx must be that source's clock/cancellation domain, not a broker
/// or another viewer's context. Retain Watch separately for actual native reap.
/// This adapter cannot be rebound to a new agent, even with the same numeric ID.
#[must_use = "service on the OS-owner loop and retain the cleanup receipt"]
pub struct Events {
    control: Control,
    identity: AgentIdentity,
    os_session: u32,
    cx: Cx,
    last: HostInstant,
    cause: Option<Error>,
    cleanup: Option<Cleanup>,
}
impl Events {
    pub fn new(control: Control, agent: &SessionAgent, cx: Cx) -> Result<Self, Error> {
        if agent.permissions().platform() != PlatformKind::LinuxX11 {
            return Err(Error::UnsupportedPlatform);
        }
        if agent.is_revoked() {
            return Err(Error::AlreadyRevoked);
        }
        if agent.permissions().is_locked() {
            return Err(Error::SessionLocked);
        }
        evidence(control.status())?;
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let last = input_watchdog::host_now(&cx).map_err(|_| Error::Clock)?;
        Ok(Self {
            control,
            identity: agent.identity(),
            os_session: agent.permissions().os_session_id(),
            cx,
            last,
            cause: None,
            cleanup: None,
        })
    }
    pub fn cause(&self) -> Option<Error> {
        self.cause
    }
    pub fn cleanup(&self) -> Option<&Cleanup> {
        self.cleanup.as_ref()
    }
    /// Transfers the original release batch once; later polls never synthesize a
    /// second batch or replace its independently observable cleanup receipt.
    pub fn take_cleanup(&mut self) -> Option<Cleanup> {
        self.cleanup.take()
    }

    /// Nonblocking local-event callback for the existing desktop open/run/serve
    /// loops. They poll this before renewal/native work and on bounded maintenance.
    /// Also use `start_x11_guarded` at final OS submission; no event-loop scheduling
    /// assumption authorizes input after a negative native signal.
    pub fn service(
        &mut self,
        agent: &mut SessionAgent,
        task: &mut Context<'_>,
    ) -> Result<LocalAction, Error> {
        if !self.identity.matches(agent) {
            // Do not revoke a different local agent. Fence the originally supplied
            // source context and evidence instead; never silently retarget.
            self.control.stop();
            self.cx.cancel_fast(CancelKind::User);
            return Err(Error::WrongAgent);
        }
        if let Some(cause) = self.cause {
            return Err(cause);
        }
        self.control.register(task);
        let current = input_watchdog::host_now(&self.cx).map_err(|_| Error::Clock);
        let checked = (|| {
            let now = current?;
            if now < self.last {
                return Err(Error::Clock);
            }
            self.last = now;
            self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
            if agent.permissions().os_session_id() != self.os_session {
                return Err(Error::SessionChanged);
            }
            if self.identity.is_revoked() {
                return Err(Error::AlreadyRevoked);
            }
            if agent.permissions().is_locked() {
                return Err(Error::SessionLocked);
            }
            evidence(self.control.status())
        })();
        let Err(cause) = checked else {
            return Ok(LocalAction::Continue);
        };
        self.cause = Some(cause);
        // Only an actual lock event changes lock state. Missing evidence still
        // revokes, but is not mislabelled as proof that the OS has locked.
        if matches!(cause, Error::Evidence(StopReason::Locked)) {
            agent.permissions_mut().on_session_locked();
        }
        let reason = if matches!(
            cause,
            Error::SessionLocked | Error::Evidence(StopReason::Locked | StopReason::Suspending)
        ) {
            input_watchdog::StopReason::Suspended
        } else {
            input_watchdog::StopReason::AuthorityEnded
        };
        let (outcome, releases) = agent.immediate_revoke(self.last, reason);
        self.cleanup = Some(Cleanup {
            cause,
            outcome,
            releases,
        });
        // Fence before returning to the outer source loop; native thread and
        // held-input cleanup remain separately owned and explicitly observable.
        self.control.stop();
        self.cx.cancel_fast(CancelKind::User);
        Err(cause)
    }
    /// Borrowing callback keeps the adapter/cleanup receipt available after the
    /// native desktop future ends or is dropped. No cleanup is discarded here.
    pub fn callback(
        &mut self,
    ) -> impl FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + '_
    {
        |agent, task| self.service(agent, task).map_err(|_| ())
    }
}
fn evidence(status: Status) -> Result<(), Error> {
    match status {
        Status::Active => Ok(()),
        Status::Opening => Err(Error::Opening),
        Status::Stopped(reason) => Err(Error::Evidence(reason)),
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        // Abandonment removes this local evidence, never creates replacement
        // permission. The original source/input owners still perform cleanup.
        self.control.stop();
        self.cx.cancel_fast(CancelKind::User);
    }
}
