//! One source-agent-bound native approval slot for existing Host notifications.
use super::{
    Approval, Control, Denial, Error, Outcome, Prompt, PromptControl, Role, SessionStatus,
};
use frd::{
    input_agent::Seat,
    session_agent::{AgentIdentity, PlatformKind, SessionAgent},
};
use std::sync::{Arc, Mutex};

struct Slot {
    closed: bool,
    pending: Option<Approval>,
    prompt: Option<Prompt>,
}
struct State {
    session: Control,
    agent: AgentIdentity,
    os_session: u32,
    input: Option<Seat>,
    slot: Mutex<Slot>,
}
impl State {
    fn stop(&self) {
        let (pending, prompt) = {
            let mut slot = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.closed = true;
            (
                slot.pending.clone(),
                slot.prompt.as_ref().map(Prompt::control),
            )
        };
        // Denial may wake the original session. Never run wake/fence callbacks
        // under the slot mutex, including during a reentrant input-stop hook.
        if let Some(approval) = pending {
            let _ = approval.decide(false);
        }
        if let Some(control) = prompt {
            control.cancel();
        }
    }
    fn reserve(&self, approval: &Approval) -> Result<PendingPrompt<'_>, Error> {
        let mut slot = self.slot.try_lock().map_err(|_| Error::Busy)?;
        if slot.closed {
            return Err(Error::Closed);
        }
        if slot.prompt.is_some() || slot.pending.is_some() {
            return Err(Error::Busy);
        }
        slot.pending = Some(approval.clone());
        Ok(PendingPrompt {
            state: self,
            active: true,
        })
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
        // Reserve one pending capability, then release the slot BEFORE the
        // Seat stop hook runs. Reentrant stop can deny this exact request;
        // concurrent notifications still cannot claim its occupied slot.
        let pending = self.reserve(&approval)?;
        let prompt = Prompt::start_scoped(
            self.session.clone(),
            approval,
            Some(self.agent.clone()),
            self.input.as_ref(),
        )?;
        let control = prompt.control();
        if pending.publish(prompt) {
            control.cancel();
            return Err(Error::Closed);
        }
        denial.0 = None;
        Ok(())
    }
}

/// One synchronous startup reservation. Failures and unwinding deny and retire
/// only its own pending capability. A completed Prompt stays collectable even
/// when a concurrent stop already denied its original request.
struct PendingPrompt<'a> {
    state: &'a State,
    active: bool,
}
impl PendingPrompt<'_> {
    fn publish(mut self, prompt: Prompt) -> bool {
        let (closed, pending) = {
            let mut slot = self
                .state
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let pending = slot.pending.take();
            slot.prompt = Some(prompt);
            (slot.closed, pending)
        };
        self.active = false;
        drop(pending);
        closed
    }
}
impl Drop for PendingPrompt<'_> {
    fn drop(&mut self) {
        if self.active {
            let pending = self
                .state
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending
                .take();
            if let Some(approval) = pending {
                let _ = approval.decide(false);
            }
        }
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
    /// A control-enabled agent supplies its canonical Seat automatically. An
    /// application with a separately managed input owner must explicitly bind
    /// that Seat with `with_input_seat`; a new or unrelated Seat is not a fence.
    pub fn new(session: Control, agent: &SessionAgent) -> Result<Self, Error> {
        let input = agent
            .control_profile()
            .map(frd::session_agent::source::desktop::ControlProfile::seat);
        Self::new_scoped(session, agent, input)
    }
    /// For a caller managing the canonical input Seat outside `ControlProfile`.
    /// Every controller of this OS share must use this exact seat. The binding
    /// is fixed for the UI lifetime and cannot be replaced by a peer request.
    pub fn with_input_seat(
        session: Control,
        agent: &SessionAgent,
        seat: Seat,
    ) -> Result<Self, Error> {
        if agent
            .control_profile()
            .is_some_and(|p| !p.seat().same_owner(&seat))
        {
            return Err(Error::WrongInputSeat);
        }
        Self::new_scoped(session, agent, Some(seat))
    }
    fn new_scoped(
        session: Control,
        agent: &SessionAgent,
        input: Option<Seat>,
    ) -> Result<Self, Error> {
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
            input,
            slot: Mutex::new(Slot {
                closed: false,
                pending: None,
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
