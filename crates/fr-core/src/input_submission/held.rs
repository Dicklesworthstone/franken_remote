//! Release-only reconciliation inside the existing serialized submission owner.
use super::{InputSession, InputSink, Operation, PreparedSink, Refusal, Submission};
use crate::{
    authority::AuthorityError,
    held_state::{HeldState, HeldStateRequest},
    input::{KeyTransition, PhysicalKey, PointerButton},
    input_sequence::InputSequenceError,
    time::HostInstant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciliationOutcome {
    Applied,
    /// Foreign/revoked lease or obsolete state: no native call was made.
    Ignored,
    Refused,
    /// Preparation or a final authority check was interrupted, not OS success.
    Interrupted,
    /// A release entered a native call whose effect is not known.
    EffectUnknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconciliation {
    pub sequence: u64,
    pub outcome: ReconciliationOutcome,
    pub submitted_releases: u16,
    /// Releases requested by this snapshot that have not been confirmed.
    pub remaining_releases: u16,
    pub refusal: Option<Refusal>,
}
impl InputSession {
    /// The latest actually attempted reconciliation, including a confirmed
    /// prefix retained before a native panic. No key/button identities escape.
    pub const fn retained_reconciliation(&self) -> Option<Reconciliation> {
        self.reconciliation
    }

    /// Apply only differences that RELEASE input owned by this lease. Tickets
    /// are deliberately not required for cleanup. Current lease, independent
    /// revocation, clock and reliable-action position are still checked.
    pub fn reconcile_held(
        &mut self,
        request: HeldStateRequest,
        sink: &mut impl InputSink,
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<Reconciliation, Refusal> {
        let ignored = Reconciliation {
            sequence: request.sequence,
            outcome: ReconciliationOutcome::Ignored,
            submitted_releases: 0,
            remaining_releases: 0,
            refusal: None,
        };
        if request.session != self.session
            || request.lease != self.lease
            || self.check_active().is_err()
            || self
                .reconciliation_floor
                .is_some_and(|floor| request.sequence <= floor)
        {
            return Ok(ignored);
        }
        let Some(expected) = self.ledger.next_sequence() else {
            return Ok(ignored);
        };
        if request.next_action < expected {
            return Ok(ignored);
        }
        if request.next_action > expected || self.ledger.pending_sequence().is_some() {
            self.revoke();
            return Err(Refusal::Sequence(InputSequenceError::SequenceGap {
                expected,
                received: request.next_action,
            }));
        }
        // Consume before callbacks/native work. Replaying an interrupted snapshot
        // cannot re-enter an uncertain release, even after newer state arrives.
        self.reconciliation_floor = Some(request.sequence);
        let (operations, count) = self.missing_releases(request.held);
        self.reconciliation = Some(Reconciliation {
            sequence: request.sequence,
            outcome: ReconciliationOutcome::Interrupted,
            submitted_releases: 0,
            remaining_releases: u16::try_from(count).expect("fixed held-state bound"),
            refusal: None,
        });
        let mut guard = ReconcileGuard {
            owner: self,
            complete: false,
        };
        let result = guard.run(&operations[..count], sink, &mut clock);
        let owner = &mut *guard.owner;
        if result.is_err() {
            owner.revoke();
        }
        let report = owner
            .reconciliation
            .as_mut()
            .expect("active reconciliation");
        match result {
            Ok(()) => report.outcome = ReconciliationOutcome::Applied,
            Err(error) => {
                if report.outcome != ReconciliationOutcome::EffectUnknown {
                    report.outcome = ReconciliationOutcome::Refused;
                }
                report.refusal = Some(error);
            }
        }
        let report = *report;
        guard.complete = true;
        Ok(report)
    }
    fn missing_releases(&self, held: HeldState) -> ([Option<Operation>; 261], usize) {
        let mut operations = [None; 261];
        let mut count = 0;
        for usage in 0_u16..256 {
            if let Some(key) = PhysicalKey::new(usage)
                && self.keys[usize::from(usage)]
                && !held.key(key)
            {
                operations[count] = Some(Operation::Key {
                    key,
                    transition: KeyTransition::Release,
                });
                count += 1;
            }
        }
        for button in [
            PointerButton::Primary,
            PointerButton::Secondary,
            PointerButton::Middle,
            PointerButton::Back,
            PointerButton::Forward,
        ] {
            if self.buttons[button as usize - 1] && !held.button(button) {
                operations[count] = Some(Operation::Button {
                    button,
                    pressed: false,
                });
                count += 1;
            }
        }
        (operations, count)
    }
    fn check_reconciliation(&self, now: HostInstant) -> Result<(), Refusal> {
        self.check_active()?;
        self.authority.with(|a| {
            if a.has_live_control(now) {
                Ok(())
            } else {
                Err(AuthorityError::NoLease)
            }
        })
    }
}

struct ReconcileGuard<'a> {
    owner: &'a mut InputSession,
    complete: bool,
}
impl Drop for ReconcileGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.owner.revoke();
        }
    }
}

impl ReconcileGuard<'_> {
    fn run(
        &mut self,
        operations: &[Option<Operation>],
        sink: &mut impl InputSink,
        clock: &mut impl FnMut() -> HostInstant,
    ) -> Result<(), Refusal> {
        let owner = &mut *self.owner;
        owner.check_reconciliation(clock())?;
        for op in operations.iter().flatten().copied() {
            let prepared = PreparedSink(sink);
            prepared.0.prepare(op).map_err(Refusal::Platform)?;
            owner.check_reconciliation(clock())?;
            // Preserve the prefix before entering an irreversible release.
            owner
                .reconciliation
                .as_mut()
                .expect("active reconciliation")
                .outcome = ReconciliationOutcome::EffectUnknown;
            match prepared.0.submit(op) {
                Submission::Submitted => {
                    match op {
                        Operation::Key { key, .. } => {
                            owner.keys[usize::from(key.usage())] = false;
                        }
                        Operation::Button { button, .. } => {
                            owner.buttons[button as usize - 1] = false;
                        }
                        _ => unreachable!("only release operations are constructed"),
                    }
                    let report = owner
                        .reconciliation
                        .as_mut()
                        .expect("active reconciliation");
                    report.submitted_releases += 1;
                    report.remaining_releases -= 1;
                    report.outcome = ReconciliationOutcome::Interrupted;
                }
                Submission::NotSubmitted(error) => {
                    owner
                        .reconciliation
                        .as_mut()
                        .expect("active reconciliation")
                        .outcome = ReconciliationOutcome::Refused;
                    return Err(Refusal::Platform(error));
                }
                Submission::Unknown => return Err(Refusal::UnknownEffect),
            }
        }
        Ok(())
    }
}
