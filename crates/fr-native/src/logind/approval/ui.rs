//! One source-agent-bound native approval slot for existing Host notifications.
use super::{
    Approval, Control, Denial, Error, Outcome, Prompt, PromptControl, Role, SessionStatus,
};
use frd::session_agent::{AgentIdentity, PlatformKind, SessionAgent};
use std::sync::{Arc, Mutex};

struct Slot {
    closed: bool,
    prompt: Option<Prompt>,
}
struct State {
    session: Control,
    agent: AgentIdentity,
    os_session: u32,
    slot: Mutex<Slot>,
}
impl State {
    fn stop(&self) {
        // No native call under this lock: cancellation only consumes the original
        // decision. Native resources stay in the slot for explicit collection.
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.closed = true;
        if let Some(prompt) = &slot.prompt {
            prompt.control().cancel();
        }
    }
    fn request(&self, approval: Approval, role: Role) -> Result<(), Error> {
        let mut denial = Denial(Some(approval.clone()));
        if role != approval.role() {
            return Err(Error::RoleMismatch);
        }
        if approval.binding().os_session.as_raw() != u128::from(self.os_session) {
            return Err(Error::WrongSession);
        }
        if self.agent.is_revoked() {
            return Err(Error::AgentUnavailable);
        }
        // No queuing behind another notification/collector or retiring window.
        let mut slot = self.slot.try_lock().map_err(|_| Error::Busy)?;
        if slot.closed {
            return Err(Error::Closed);
        }
        if slot.prompt.is_some() {
            return Err(Error::Busy);
        }
        let prompt =
            Prompt::start_scoped(self.session.clone(), approval, Some(self.agent.clone()))?;
        slot.prompt = Some(prompt);
        denial.0 = None;
        Ok(())
    }
}

/// Supplies the native handler for `Host::open`, shared `Incoming::serve_host`
/// and the existing desktop Driver APIs. Notification is NOT an approval.
/// Keep this owner on the selected user's independent OS-source lifetime, and
/// keep the original logind Watch/Events and sharing indicator independently.
/// There is one prompt and no implicit queue, retry or permission fallback.
#[must_use = "retain local approval ownership and collect native retirement"]
pub struct ApprovalUi(Arc<State>);
impl ApprovalUi {
    pub fn new(session: Control, agent: &SessionAgent) -> Result<Self, Error> {
        if agent.permissions().platform() != PlatformKind::LinuxX11
            || agent.permissions().is_locked()
            || agent.is_revoked()
        {
            return Err(Error::AgentUnavailable);
        }
        if !session.matches_local_x11(&session.0.selection.display) {
            return Err(Error::WrongSession);
        }
        if session.status() != SessionStatus::Active {
            return Err(Error::SessionUnavailable);
        }
        let state = Arc::new(State {
            session,
            agent: agent.identity(),
            os_session: agent.permissions().os_session_id(),
            slot: Mutex::new(Slot {
                closed: false,
                prompt: None,
            }),
        });
        let weak = Arc::downgrade(&state);
        // Only the original agent can invoke this registration. A replacement
        // with the same numeric OS ID does not inherit this UI or pending choice.
        agent.indicator().register_custom_revoker(move || {
            if let Some(state) = weak.upgrade() {
                state.stop();
            }
        });
        Ok(Self(state))
    }
    /// A weak notification closure usable by the canonical Send/'static callback
    /// APIs. Keeping a callback never keeps a discarded UI/native thread alive.
    /// Failure denies the supplied request; no prior decision is reused.
    pub fn callback(
        &self,
    ) -> impl FnMut(Approval, Role) -> Result<(), ()> + Send + 'static + use<> {
        let weak = Arc::downgrade(&self.0);
        move |approval, role| {
            let Some(state) = weak.upgrade() else {
                let _ = approval.decide(false);
                return Err(());
            };
            state.request(approval, role).map_err(|_| ())
        }
    }
    /// Original pending/retiring prompt only. A clone cannot decide positively.
    pub fn current(&self) -> Option<PromptControl> {
        self.0
            .slot
            .lock()
            .ok()?
            .prompt
            .as_ref()
            .map(Prompt::control)
    }
    /// Collect one actual thread-retirement receipt. Until collected, even an
    /// Allowed/Denied prompt occupies its slot and another request is refused.
    /// This never waits on a running foreign call and never repeats the receipt.
    pub fn collect(&mut self) -> Option<Outcome> {
        let mut slot = self.0.slot.try_lock().ok()?;
        let outcome = slot.prompt.as_mut()?.try_finish()?;
        drop(slot.prompt.take());
        Some(outcome)
    }
    /// Permanently deny pending/future requests; collect original native cleanup
    /// separately. Already committed consent is not retroactively undone here.
    pub fn stop(&self) {
        self.0.stop();
    }
}
impl std::fmt::Debug for ApprovalUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApprovalUi([original local agent])")
    }
}
impl Drop for ApprovalUi {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
