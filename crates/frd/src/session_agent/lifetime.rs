//! Opaque association for local adapters; not a peer identity or authority grant.
use super::SessionAgent;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};

/// A non-owning brand for this exact agent, independent of numeric OS/session IDs.
/// It cannot create or renew authority and does not keep a discarded agent alive.
#[derive(Clone)]
pub struct AgentIdentity(Weak<AtomicBool>);
impl std::fmt::Debug for AgentIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgentIdentity([local lifetime])")
    }
}
impl AgentIdentity {
    pub fn matches(&self, agent: &SessionAgent) -> bool {
        self.0.ptr_eq(&Arc::downgrade(&agent.revoked))
    }
    pub fn is_revoked(&self) -> bool {
        self.0
            .upgrade()
            .is_none_or(|state| state.load(Ordering::Acquire))
    }
}
impl SessionAgent {
    pub fn identity(&self) -> AgentIdentity {
        AgentIdentity(Arc::downgrade(&self.revoked))
    }
}
