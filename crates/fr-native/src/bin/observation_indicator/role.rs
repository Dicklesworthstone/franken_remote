//! The macOS helper's single observation role. Matching a sequence and epoch
//! is only correlation; neither an approval nor another UI role is accepted.
use fr_core::indicator_process::{Frame, Kind};

pub(crate) struct Observation {
    epoch: u128,
    next: u64,
}

impl Observation {
    pub(crate) fn open(frame: Frame) -> Option<Self> {
        (frame.kind == Kind::Open && frame.sequence == 1 && frame.epoch != 0).then_some(Self {
            epoch: frame.epoch,
            next: 2,
        })
    }

    pub(crate) fn accept(&mut self, frame: Frame) -> Option<Kind> {
        if frame.epoch != self.epoch
            || frame.sequence != self.next
            || !matches!(frame.kind, Kind::Check | Kind::Stop)
        {
            return None;
        }
        self.next = self.next.checked_add(1)?;
        Some(frame.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Frame {
        Frame {
            kind: Kind::Open,
            sequence: 1,
            epoch: 7,
        }
    }

    #[test]
    fn only_an_original_observation_open_can_create_a_role() {
        let valid = open();
        assert!(Observation::open(valid).is_some());
        for kind in [
            Kind::Check,
            Kind::Stop,
            Kind::OpenMicrophone,
            Kind::ApproveView,
            Kind::ApproveControl,
            Kind::ApprovalCheck,
            Kind::Ready,
            Kind::Allowed,
        ] {
            assert!(Observation::open(Frame { kind, ..valid }).is_none());
        }
        assert!(Observation::open(Frame { epoch: 0, ..valid }).is_none());
        assert!(Observation::open(Frame { sequence: 2, ..valid }).is_none());
    }

    #[test]
    fn duplicate_skipped_wrong_epoch_and_other_roles_are_refused() {
        let original = open();
        let check = Frame {
            kind: Kind::Check,
            sequence: 2,
            ..original
        };
        let mut role = Observation::open(original).unwrap();
        for foreign in [
            Frame { sequence: 1, ..check },
            Frame { sequence: 3, ..check },
            Frame { epoch: 8, ..check },
            Frame { kind: Kind::CheckMicrophone, ..check },
            Frame { kind: Kind::ApprovalCheck, ..check },
            Frame { kind: Kind::Ready, ..check },
            Frame { kind: Kind::Open, ..check },
        ] {
            assert_eq!(role.accept(foreign), None);
        }
        assert_eq!(role.accept(check), Some(Kind::Check));
        assert_eq!(role.accept(check), None);
        assert_eq!(
            role.accept(Frame { kind: Kind::Stop, sequence: 3, ..original }),
            Some(Kind::Stop)
        );
    }

    #[test]
    fn sequence_cannot_wrap_into_another_lifetime() {
        let mut role = Observation { epoch: 7, next: u64::MAX };
        assert_eq!(
            role.accept(Frame { kind: Kind::Check, sequence: u64::MAX, ..open() }),
            None
        );
    }
}
