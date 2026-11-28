//! The strong owner of one browser observation consent. Native UI retains only
//! the existing weak Approval capability, so socket cancellation retires it.
use super::{ALLOWED, Approval, CONSUMED, DENIED, Error, RETIRED, WAITING, approval_context, now};
use asupersync::cx::Cx;
use fr_tailnet::Lease;
use fr_wire::negotiation::{ControlBinding, Role, Selection, browser};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

pub(crate) struct PendingApproval {
    state: Arc<AtomicU8>,
    original: Approval,
}
impl PendingApproval {
    pub(crate) fn browser(
        cx: Cx,
        peer: &Lease,
        binding: ControlBinding,
        selection: &Selection,
        deadline: u64,
    ) -> Result<Self, Error> {
        let current = now(&cx)?;
        peer.observe().map_err(Error::Admission)?;
        if selection.role != Role::Observe
            || !browser::local_approval_selected(selection)
            || binding.id == 0
            || binding.host_boot.as_raw() == 0
            || binding.os_session.as_raw() == 0
            || binding.remote_session.as_raw() == 0
            || deadline <= current
            || deadline - current > browser::APPROVAL_STARTUP_BUDGET_US
        {
            return Err(Error::InvalidConfiguration);
        }
        let attribution = Arc::new(approval_context::Attribution::from_lease(peer, selection)?);
        let state = Arc::new(AtomicU8::new(WAITING));
        let original = Approval {
            binding,
            role: selection.role,
            state: Arc::downgrade(&state),
            cx,
            deadline,
            attribution: Some(attribution),
        };
        original.check_pending()?;
        Ok(Self { state, original })
    }
    pub(crate) fn capability(&self) -> Result<Approval, Error> {
        self.original.check_pending()?;
        Ok(self.original.clone())
    }
    /// The caller checks the same original peer and local policy before AND
    /// after this operation. A positive answer never grants authority itself.
    pub(crate) fn consume(&mut self) -> Result<bool, Error> {
        if now(&self.original.cx)? >= self.original.deadline {
            return Err(Error::Expired);
        }
        match self.state.load(Ordering::Acquire) {
            WAITING => Ok(false),
            ALLOWED => self
                .state
                .compare_exchange(ALLOWED, CONSUMED, Ordering::AcqRel, Ordering::Acquire)
                .map(|_| true)
                .map_err(|_| Error::Denied),
            DENIED => Err(Error::Denied),
            RETIRED => Err(Error::Closed),
            _ => Err(Error::Order),
        }
    }
}
impl Drop for PendingApproval {
    fn drop(&mut self) {
        self.state.store(RETIRED, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{
        runtime::RuntimeBuilder,
        types::{Budget, CancelKind},
    };
    use fr_core::ids::{HostBootId, OsSessionId, RemoteSessionId};

    // Capability-state fixtures only; these do not claim daemon admission or
    // physical UI consent. Production construction requires the original Lease.
    fn pending(cx: &Cx) -> PendingApproval {
        let state = Arc::new(AtomicU8::new(WAITING));
        let original = Approval {
            binding: ControlBinding {
                id: 1,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            role: Role::Observe,
            state: Arc::downgrade(&state),
            cx: cx.clone(),
            deadline: now(cx).unwrap() + browser::APPROVAL_STARTUP_BUDGET_US,
            attribution: None,
        };
        PendingApproval { state, original }
    }
    #[test]
    fn consent_is_pending_until_one_positive_original_decision_is_consumed() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let mut owner = pending(&cx);
        let decision = owner.capability().unwrap();
        assert_eq!(owner.consume(), Ok(false));
        decision.decide(true).unwrap();
        assert_eq!(owner.consume(), Ok(true));
        assert_eq!(owner.consume(), Err(Error::Order));
        assert!(decision.decide(true).is_err());
        assert!(owner.capability().is_err());
    }
    #[test]
    fn a_retired_request_cannot_approve_an_equal_numbered_replacement() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let original = pending(&cx);
        let escaped = original.capability().unwrap();
        let mut replacement = pending(&cx);
        assert_eq!(escaped.binding(), replacement.original.binding());
        drop(original);
        assert_eq!(escaped.decide(true), Err(Error::Closed));
        assert_eq!(replacement.consume(), Ok(false));
        replacement.capability().unwrap().decide(false).unwrap();
        assert_eq!(replacement.consume(), Err(Error::Denied));
    }
    #[test]
    fn expired_or_cancelled_originals_reject_even_a_delivered_positive_decision() {
        for cancelled in [false, true] {
            let runtime = RuntimeBuilder::current_thread()
                .enable_platform_reactor(true)
                .build()
                .unwrap();
            let cx = runtime.request_cx_with_budget(Budget::INFINITE);
            let mut owner = pending(&cx);
            owner.capability().unwrap().decide(true).unwrap();
            if cancelled {
                cx.cancel_fast(CancelKind::User);
                assert_eq!(owner.consume(), Err(Error::Cancelled));
            } else {
                owner.original.deadline = now(&cx).unwrap();
                assert_eq!(owner.consume(), Err(Error::Expired));
            }
        }
    }
}
