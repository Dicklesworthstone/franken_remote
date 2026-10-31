//! One causal check joining a selected capture inventory to a ready native input
//! owner. This is topology evidence only: no pixels, freshness or authority.
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

const AWAIT_NATIVE: u8 = 0;
const REQUESTED: u8 = 1;
const CHECKING: u8 = 2;
const VERIFIED: u8 = 3;
const FAILED: u8 = 4;

struct Shared {
    source: Arc<()>,
    phase: AtomicU8,
}

/// Handles share one non-resettable gate. Only the uniquely claimed, non-Clone
/// command can complete it; copying a handle cannot manufacture another check.
#[derive(Clone)]
pub(crate) struct InputTargetCheck(Arc<Shared>);

impl InputTargetCheck {
    pub(super) fn new(source: &Arc<()>) -> Self {
        Self(Arc::new(Shared {
            source: source.clone(),
            phase: AtomicU8::new(AWAIT_NATIVE),
        }))
    }

    /// Called only after the original native input owner reports Running. The
    /// first call requests new source work; existing capture replies cannot
    /// satisfy it. Failure and verification are terminal, never reset or reused.
    pub(crate) fn after_native_ready(&self) -> Result<bool, ()> {
        match self.0.phase.compare_exchange(
            AWAIT_NATIVE,
            REQUESTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(REQUESTED | CHECKING) => Ok(false),
            Err(VERIFIED) => Ok(true),
            Err(_) => Err(()),
        }
    }

    /// The source service claims at most one command, after any capture already
    /// in progress has drained. No caller can replace a lost/abandoned command.
    pub(crate) fn take_pending(&self) -> Option<PendingInputTargetCheck> {
        self.0
            .phase
            .compare_exchange(REQUESTED, CHECKING, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| PendingInputTargetCheck {
                shared: self.0.clone(),
                complete: false,
            })
    }

    /// Native work must drain before capture or a recovery handoff can follow.
    pub(crate) fn pending(&self) -> bool {
        matches!(self.0.phase.load(Ordering::Acquire), REQUESTED | CHECKING)
    }
}

/// One source-bound command, created only after native readiness. Deliberately
/// non-Clone: a failed, cancelled or dropped check cannot leave a reusable proof.
pub(crate) struct PendingInputTargetCheck {
    shared: Arc<Shared>,
    complete: bool,
}

impl PendingInputTargetCheck {
    pub(super) fn matches(&self, source: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.shared.source, source)
            && self.shared.phase.load(Ordering::Acquire) == CHECKING
    }

    /// The original source calls this only after its newly issued `CheckMonitor`
    /// reply and original observation authority have both been checked.
    pub(super) fn verified(mut self, source: &Arc<()>) -> Result<(), ()> {
        if !self.matches(source) {
            return Err(());
        }
        self.shared.phase.store(VERIFIED, Ordering::Release);
        self.complete = true;
        Ok(())
    }
}

impl Drop for PendingInputTargetCheck {
    fn drop(&mut self) {
        if !self.complete {
            self.shared.phase.store(FAILED, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_work_cannot_be_issued_before_native_readiness_or_reissued() {
        let source = Arc::new(());
        let check = InputTargetCheck::new(&source);
        assert!(!check.pending());
        assert!(check.take_pending().is_none());
        assert_eq!(check.after_native_ready(), Ok(false));
        let command = check.take_pending().unwrap();
        assert!(check.pending());
        assert_eq!(check.after_native_ready(), Ok(false));
        assert!(check.take_pending().is_none());
        command.verified(&source).unwrap();
        assert_eq!(check.after_native_ready(), Ok(true));
        assert!(!check.pending());
        assert!(check.take_pending().is_none());
    }

    #[test]
    fn a_replacement_source_cannot_validate_the_original_command() {
        let check = InputTargetCheck::new(&Arc::new(()));
        assert_eq!(check.after_native_ready(), Ok(false));
        assert!(
            check
                .take_pending()
                .unwrap()
                .verified(&Arc::new(()))
                .is_err()
        );
        assert_eq!(check.after_native_ready(), Err(()));
        assert!(check.take_pending().is_none());
    }

    #[test]
    fn abandoning_queued_or_running_work_permanently_refuses_the_grant() {
        let source = Arc::new(());
        let check = InputTargetCheck::new(&source);
        assert_eq!(check.after_native_ready(), Ok(false));
        drop(check.take_pending().unwrap());
        assert_eq!(check.after_native_ready(), Err(()));
        assert!(check.clone().take_pending().is_none());
    }

    #[test]
    fn a_later_grant_cannot_reuse_a_verified_check_for_the_same_source() {
        let source = Arc::new(());
        let old = InputTargetCheck::new(&source);
        assert_eq!(old.after_native_ready(), Ok(false));
        old.take_pending().unwrap().verified(&source).unwrap();
        let next = InputTargetCheck::new(&source);
        assert_eq!(next.after_native_ready(), Ok(false));
        assert_eq!(old.after_native_ready(), Ok(true));
        assert_eq!(next.after_native_ready(), Ok(false));
    }
}
