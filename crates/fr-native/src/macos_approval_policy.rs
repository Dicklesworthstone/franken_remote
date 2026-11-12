//! One original native consent gesture and its bounded visibility evidence.
//! Native event metadata is evidence, never a reusable approval capability.
#![forbid(unsafe_code)]

pub(crate) const OPEN_NS: u64 = 2_000_000_000;
pub(crate) const VALID_NS: u64 = 250_000_000;
const PROMPT_NS: u64 = 30_000_000_000;
const REMOTE_TAG: i64 = 0x4652_4350;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Shown,
    /// A native decision remains tentative until the exact child is reaped
    /// and the broker consumes its original, still-pending Approval.
    Decided(bool),
    Refused,
}

/// Scalar facts from one native mouse event. The native boundary has already
/// checked the exact window/view, focused visible surface and hit rectangle.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Event {
    /// 1: Allow down; 2: Allow up; 3: interrupted gesture.
    pub kind: u32,
    pub reserved: u32,
    pub number: u64,
    pub at_ns: u64,
    pub source_pid: i64,
    pub source_state: i64,
    pub user_data: i64,
}

/// At most two gesture events per completed native turn. Saturation or loss
/// of the original surface is a refusal, never dropping an unknown transition.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Native {
    /// 0: opening; 1: focused and drawn; 2: local deny; 3: native failure.
    pub state: u32,
    pub count: u32,
    /// Native event timestamps and these two samples use the same uptime clock.
    pub ready_ns: u64,
    pub sampled_ns: u64,
    pub events: [Event; 2],
}

pub(crate) struct Evidence {
    born: u64,
    last: u64,
    until: u64,
    deadline: u64,
    last_started: Option<u64>,
    ready: Option<u64>,
    pressed: Option<(u64, u64)>,
    status: Status,
}
impl Evidence {
    pub(crate) fn new(now: Option<u64>) -> Self {
        let born = now.unwrap_or(0);
        let until = born.checked_add(OPEN_NS);
        let deadline = born.checked_add(PROMPT_NS);
        Self {
            born,
            last: born,
            until: until.unwrap_or(0),
            deadline: deadline.unwrap_or(0),
            last_started: None,
            ready: None,
            pressed: None,
            status: if now.is_some() && until.is_some() && deadline.is_some() {
                Status::Opening
            } else {
                Status::Refused
            },
        }
    }

    pub(crate) fn refuse(&mut self) -> Status {
        // A negative decision cannot become positive. A tentative yes can
        // still fail on cancellation, expiry or native destruction failure.
        if self.status != Status::Decided(false) {
            self.status = Status::Refused;
        }
        self.pressed = None;
        self.status
    }

    pub(crate) fn status(&mut self, now: Option<u64>) -> Status {
        if matches!(self.status, Status::Refused | Status::Decided(false)) {
            return self.status;
        }
        let Some(now) = now else {
            return self.refuse();
        };
        if now < self.last || now >= self.until || now >= self.deadline {
            return self.refuse();
        }
        self.last = now;
        self.status
    }

    pub(crate) fn complete(&mut self, started: u64, now: Option<u64>, native: Native) -> Status {
        if !matches!(self.status(now), Status::Opening | Status::Shown) {
            return self.status;
        }
        let Some(now) = now else {
            return self.refuse();
        };
        if started < self.born || started > now || native.count > 2 {
            return self.refuse();
        }
        if native.state == 2 {
            self.pressed = None;
            self.status = Status::Decided(false);
            return self.status;
        }
        if native.state == 0 && self.status == Status::Opening && native.count == 0 {
            return self.status; // Never renew the original opening budget.
        }
        if native.state != 1
            || native.ready_ns == 0
            || native.ready_ns > native.sampled_ns
            || self.ready.is_some_and(|ready| ready != native.ready_ns)
        {
            return self.refuse();
        }
        let Some(until) = started.checked_add(VALID_NS) else {
            return self.refuse();
        };
        if now >= until {
            return self.refuse();
        }
        self.until = until.min(self.deadline);
        self.last_started = Some(started);
        self.ready = Some(native.ready_ns);
        self.status = Status::Shown;
        // count was checked against the fixed two-event array above.
        let count = usize::try_from(native.count).unwrap_or(0);
        for event in native.events.into_iter().take(count) {
            // State-table metadata alone is not physical provenance: an
            // application can create a HID-state source. Refuse process-
            // attributed sources too, plus our executor's tag and unknown
            // events. This is not a defense against the trusted desktop user
            // forging native events; actual local input remains a native gate.
            let local = event.source_pid == 0
                && event.source_state == 1
                && event.user_data != REMOTE_TAG
                && event.reserved == 0
                && event.at_ns >= native.ready_ns
                && event.at_ns <= native.sampled_ns
                && native.sampled_ns - event.at_ns < VALID_NS;
            if !local {
                self.pressed = None;
                self.status = Status::Shown;
                continue;
            }
            match event.kind {
                1 => {
                    self.pressed = Some((event.number, event.at_ns));
                    self.status = Status::Shown;
                }
                2 => {
                    if self
                        .pressed
                        .take()
                        .is_some_and(|(number, at)| number == event.number && at <= event.at_ns)
                    {
                        self.status = Status::Decided(true);
                    }
                }
                _ => {
                    self.pressed = None;
                    self.status = Status::Shown;
                }
            }
        }
        self.status
    }

    pub(crate) fn ready_since(&mut self, floor: u64, now: Option<u64>) -> bool {
        self.status(now) == Status::Shown
            && self.last_started.is_some_and(|started| started >= floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u32, number: u64, at_ns: u64) -> Event {
        Event {
            kind,
            number,
            at_ns,
            source_state: 1,
            ..Event::default()
        }
    }
    fn native(events: &[Event]) -> Native {
        let mut native = Native {
            state: 1,
            ready_ns: 10,
            sampled_ns: 100,
            count: u32::try_from(events.len()).unwrap(),
            ..Native::default()
        };
        native.events[..events.len()].copy_from_slice(events);
        native
    }

    #[test]
    fn only_a_complete_fresh_original_mouse_gesture_can_decide_yes() {
        let mut evidence = Evidence::new(Some(0));
        assert_eq!(evidence.complete(1, Some(2), native(&[])), Status::Shown);
        assert!(evidence.ready_since(1, Some(3)));
        assert!(!evidence.ready_since(2, Some(4)));
        assert_eq!(
            evidence.complete(5, Some(6), native(&[event(1, 9, 20)])),
            Status::Shown
        );
        assert_eq!(
            evidence.complete(7, Some(8), native(&[event(2, 9, 21)])),
            Status::Decided(true)
        );
        // Native completion remains subject to the original visibility budget
        // while the exact child is closing; it is not a granted approval.
        assert_eq!(evidence.status(Some(7 + VALID_NS)), Status::Refused);
    }

    #[test]
    fn process_generated_tagged_stale_or_unmatched_events_never_approve() {
        for bad in [
            Event {
                source_pid: 42,
                ..event(1, 9, 20)
            },
            Event {
                source_state: -1,
                ..event(1, 9, 20)
            },
            Event {
                user_data: REMOTE_TAG,
                ..event(1, 9, 20)
            },
            event(1, 9, 9),
            event(1, 9, 101),
            event(3, 9, 20),
            event(1, 10, 20),
        ] {
            let mut evidence = Evidence::new(Some(0));
            assert_eq!(
                evidence.complete(1, Some(2), native(&[bad, event(2, 9, 21)])),
                Status::Shown
            );
        }
        let mut evidence = Evidence::new(Some(0));
        assert_eq!(
            evidence.complete(1, Some(2), native(&[event(2, 9, 21)])),
            Status::Shown
        );
        let mut evidence = Evidence::new(Some(0));
        let mut delayed = native(&[event(1, 9, 20), event(2, 9, 21)]);
        delayed.sampled_ns = 21 + VALID_NS;
        assert_eq!(evidence.complete(1, Some(2), delayed), Status::Shown);
    }

    #[test]
    fn denied_hidden_replaced_or_saturated_surfaces_cannot_approve() {
        let gesture = native(&[event(1, 9, 20), event(2, 9, 21)]);
        for turn in [
            Native {
                state: 0,
                ..gesture
            },
            Native {
                state: 2,
                ..gesture
            },
            Native {
                state: 3,
                ..gesture
            },
            Native {
                count: 3,
                ..gesture
            },
            Native {
                ready_ns: 11,
                ..gesture
            },
        ] {
            let mut evidence = Evidence::new(Some(0));
            assert_eq!(evidence.complete(1, Some(2), native(&[])), Status::Shown);
            assert!(matches!(
                evidence.complete(3, Some(4), turn),
                Status::Refused | Status::Decided(false)
            ));
            assert_ne!(
                evidence.complete(5, Some(6), gesture),
                Status::Decided(true)
            );
        }
    }

    #[test]
    fn an_interrupted_press_cannot_be_completed_by_a_later_release() {
        for interrupted in [
            event(3, 9, 21),
            Event {
                source_pid: 42,
                ..event(2, 9, 21)
            },
            Event {
                source_state: 0,
                ..event(2, 9, 21)
            },
            Event {
                user_data: REMOTE_TAG,
                ..event(2, 9, 21)
            },
        ] {
            let mut evidence = Evidence::new(Some(0));
            assert_eq!(
                evidence.complete(1, Some(2), native(&[event(1, 9, 20)])),
                Status::Shown
            );
            assert_eq!(
                evidence.complete(3, Some(4), native(&[interrupted])),
                Status::Shown
            );
            assert_eq!(
                evidence.complete(5, Some(6), native(&[event(2, 9, 22)])),
                Status::Shown
            );
        }
        let mut evidence = Evidence::new(Some(0));
        evidence.complete(1, Some(2), native(&[event(1, 9, 20)]));
        // An invalid extra transition in the same native turn invalidates the
        // tentative mouse-up too; it cannot leave a delivered yes behind.
        assert_eq!(
            evidence.complete(3, Some(4), native(&[event(2, 9, 21), event(3, 9, 22)])),
            Status::Shown
        );
    }

    #[test]
    fn cancellation_clocks_and_original_open_budget_are_terminal() {
        let gesture = native(&[event(1, 9, 20), event(2, 9, 21)]);
        for now in [None, Some(OPEN_NS), Some(u64::MAX)] {
            let mut evidence = Evidence::new(Some(0));
            assert_eq!(evidence.complete(1, now, gesture), Status::Refused);
        }
        let mut evidence = Evidence::new(Some(10));
        assert_eq!(evidence.status(Some(9)), Status::Refused);
        assert_eq!(evidence.complete(11, Some(12), gesture), Status::Refused);
        let mut evidence = Evidence::new(Some(0));
        evidence.refuse();
        assert_eq!(evidence.complete(1, Some(2), gesture), Status::Refused);
    }

    #[test]
    fn live_native_turns_never_extend_the_original_prompt_deadline() {
        let mut evidence = Evidence::new(Some(0));
        for started in (1..PROMPT_NS).step_by(usize::try_from(VALID_NS / 2).unwrap()) {
            assert_eq!(
                evidence.complete(started, Some(started + 1), native(&[])),
                Status::Shown
            );
        }
        assert_eq!(evidence.status(Some(PROMPT_NS)), Status::Refused);
        assert_eq!(
            evidence.complete(
                PROMPT_NS,
                Some(PROMPT_NS),
                native(&[event(1, 1, 20), event(2, 1, 21)])
            ),
            Status::Refused
        );
    }

    #[test]
    fn native_snapshot_layout_is_fixed_and_bounded() {
        assert_eq!(std::mem::size_of::<Event>(), 48);
        assert_eq!(std::mem::size_of::<Native>(), 120);
    }
}
