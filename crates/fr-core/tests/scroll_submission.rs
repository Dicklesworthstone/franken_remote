//! Production compound-scroll authorization and outcome accounting; native sink
//! and clock are deterministic fixtures here. Real X11 effects have their own lane.
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::{scroll::*, *},
    time::{HostDuration, HostInstant},
};
use std::{
    cell::Cell,
    panic::{AssertUnwindSafe, catch_unwind},
};
fn at(t: u64) -> HostInstant {
    HostInstant::from_micros(t)
}
fn setup() -> (InputSession, InputCredentials) {
    let c = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut a = SessionAuthority::new(
        c.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(3000),
            ticket_lifetime: HostDuration::from_micros(1000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.mark_view_ready(at(0)).unwrap();
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    let caps = Capabilities::default()
        .with(Capability::Absolute)
        .with(Capability::LineScroll)
        .with(Capability::PixelScroll);
    (
        InputSession::new(
            a,
            c,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            caps,
            at(0),
        )
        .unwrap(),
        c,
    )
}
fn request(c: InputCredentials, x: i32, y: i32) -> InputRequest<'static> {
    InputRequest {
        credentials: c,
        sequence: 0,
        event: InputEvent::Scroll {
            position: DesktopPoint { x: 20, y: 30 },
            barrier: 8,
            x,
            y,
            unit: ScrollUnit::Lines,
        },
    }
}
fn receipt(result: Result<Dispatch, Refusal>) -> Receipt {
    let Dispatch::Completed(r) = result.unwrap() else {
        panic!("receipt missing")
    };
    r
}
struct Sink<'a> {
    ops: Vec<Operation>,
    prepared: Option<Operation>,
    fault: Option<(usize, Submission)>,
    panic_at: Option<usize>,
    expire_at: Option<usize>,
    clock: &'a Cell<u64>,
    pairs: bool,
}
impl<'a> Sink<'a> {
    fn new(clock: &'a Cell<u64>) -> Self {
        Self {
            ops: Vec::new(),
            prepared: None,
            fault: None,
            panic_at: None,
            expire_at: None,
            clock,
            pairs: true,
        }
    }
}
impl InputSink for Sink<'_> {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.prepared = Some(op);
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        assert_eq!(self.prepared, Some(op));
        assert_ne!(self.panic_at, Some(self.ops.len()), "injected native panic");
        if let Some((n, failure)) = self.fault
            && n == self.ops.len()
        {
            return failure;
        }
        self.ops.push(op);
        if self.expire_at == Some(self.ops.len()) {
            self.clock.set(1000);
        }
        Submission::Submitted
    }
    fn cancel_prepared(&mut self) {
        self.prepared = None;
    }
    fn line_scroll_requires_pairs(&self) -> bool {
        self.pairs
    }
}
#[test]
fn exact_fixed_point_bound_and_direction_order_without_rounding_or_overflow() {
    assert_eq!(LineScroll::new(0, 0).unwrap().native_operations(), 1);
    assert_eq!(
        LineScroll::new(32 * LINE, 0).unwrap().native_operations(),
        65
    );
    assert_eq!(
        LineScroll::new(-LINE, 2 * LINE)
            .unwrap()
            .steps()
            .collect::<Vec<_>>(),
        [
            WheelDirection::Left,
            WheelDirection::Down,
            WheelDirection::Down
        ]
    );
    for (x, y) in [
        (1, 0),
        (LINE / 2, 0),
        (-LINE / 2, 0),
        (32 * LINE, LINE),
        (i32::MIN, 0),
        (i32::MAX, 0),
    ] {
        assert!(LineScroll::new(x, y).is_none());
    }
}
#[test]
fn one_reliable_scroll_expands_into_separately_authorized_transitions() {
    let (mut host, c) = setup();
    let clock = Cell::new(1);
    let mut sink = Sink::new(&clock);
    let req = request(c, -LINE, 2 * LINE);
    let r = receipt(host.dispatch(req, &mut sink, || at(clock.get())));
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(r.submitted_operations, 7);
    assert_eq!(host.held_count(), 0);
    assert_eq!(
        sink.ops[0],
        Operation::Absolute(DesktopPoint { x: 20, y: 30 })
    );
    for (pair, d) in sink.ops[1..].as_chunks::<2>().0.iter().zip([
        WheelDirection::Left,
        WheelDirection::Down,
        WheelDirection::Down,
    ]) {
        assert_eq!(
            *pair,
            [
                Operation::Wheel {
                    direction: d,
                    pressed: true
                },
                Operation::Wheel {
                    direction: d,
                    pressed: false
                }
            ]
        );
    }
    assert_eq!(
        receipt(host.dispatch(req, &mut sink, || at(clock.get()))),
        r
    );
    assert_eq!(sink.ops.len(), 7);
}
#[test]
fn fractional_and_excessive_native_work_refuse_before_positioning() {
    for (x, y) in [
        (1, 0),
        (-1, 0),
        (LINE + 1, 0),
        (33 * LINE, 0),
        (16 * LINE, 17 * LINE),
        (i32::MIN, 0),
    ] {
        let (mut host, c) = setup();
        let clock = Cell::new(1);
        let mut sink = Sink::new(&clock);
        let r = receipt(host.dispatch(request(c, x, y), &mut sink, || at(1)));
        assert_eq!(r.outcome, InputOutcome::RejectedBeforeSubmission);
        assert_eq!(r.refusal, Some(Refusal::Unsupported));
        assert_eq!(sink.ops, []);
        assert_eq!(host.held_count(), 0);
    }
}
#[test]
fn zero_scroll_positions_without_synthetic_wheel_activity() {
    let (mut host, c) = setup();
    let clock = Cell::new(1);
    let mut sink = Sink::new(&clock);
    let r = receipt(host.dispatch(request(c, 0, 0), &mut sink, || at(1)));
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(sink.ops.len(), 1);
    assert_eq!(host.held_count(), 0);
}
#[test]
fn every_partial_native_prefix_retains_only_release_cleanup_not_future_steps() {
    for cutoff in 1..5 {
        let (mut host, c) = setup();
        let clock = Cell::new(1);
        let mut sink = Sink::new(&clock);
        sink.expire_at = Some(cutoff);
        let req = request(c, LINE, LINE);
        let r = receipt(host.dispatch(req, &mut sink, || at(clock.get())));
        assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
        assert_eq!(r.submitted_operations, u32::try_from(cutoff).unwrap());
        assert_eq!(host.held_count(), u16::from(cutoff % 2 == 0));
        assert!(sink.prepared.is_none());
        let cleanup = host.cleanup(&mut sink);
        assert_eq!(cleanup.remaining, 0);
        assert_eq!(
            usize::from(cleanup.submitted_releases),
            usize::from(cutoff % 2 == 0)
        );
        for effect in &sink.ops[cutoff..] {
            assert!(matches!(effect, Operation::Wheel { pressed: false, .. }));
        }
        let effects = sink.ops.len();
        assert_eq!(receipt(host.dispatch(req, &mut sink, || at(1001))), r);
        assert_eq!(sink.ops.len(), effects);
    }
}
#[test]
fn unknown_press_retains_possible_hold_and_known_position_prefix() {
    let (mut host, c) = setup();
    let clock = Cell::new(1);
    let mut sink = Sink::new(&clock);
    sink.fault = Some((1, Submission::Unknown));
    let req = request(c, 0, LINE);
    let r = receipt(host.dispatch(req, &mut sink, || at(1)));
    assert_eq!(r.outcome, InputOutcome::EffectUnknown);
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(host.held_count(), 1);
    sink.fault = None;
    let cleanup = host.cleanup(&mut sink);
    assert_eq!(cleanup.submitted_releases, 1);
    assert_eq!(cleanup.remaining, 0);
    assert_eq!(
        sink.ops.last(),
        Some(&Operation::Wheel {
            direction: WheelDirection::Down,
            pressed: false
        })
    );
    assert_eq!(receipt(host.dispatch(req, &mut sink, || at(2))), r);
}
#[test]
fn rejected_press_does_not_invent_a_held_wheel() {
    let (mut host, c) = setup();
    let clock = Cell::new(1);
    let mut sink = Sink::new(&clock);
    sink.fault = Some((1, Submission::NotSubmitted(PlatformError::Permission)));
    let r = receipt(host.dispatch(request(c, 0, LINE), &mut sink, || at(1)));
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(host.held_count(), 0);
    assert_eq!(host.cleanup(&mut sink).submitted_releases, 0);
}
#[test]
fn native_panic_preserves_uncertain_wheel_ownership_and_receipt() {
    let (mut host, c) = setup();
    let clock = Cell::new(1);
    let mut sink = Sink::new(&clock);
    sink.panic_at = Some(1);
    assert!(
        catch_unwind(AssertUnwindSafe(|| host.dispatch(
            request(c, 0, -LINE),
            &mut sink,
            || at(1)
        )))
        .is_err()
    );
    assert_eq!(host.held_count(), 1);
    let r = host.retained_receipt(0).unwrap();
    assert_eq!(r.outcome, InputOutcome::EffectUnknown);
    assert_eq!(r.submitted_operations, 1);
    assert!(sink.prepared.is_none());
    sink.panic_at = None;
    assert_eq!(host.cleanup(&mut sink).submitted_releases, 1);
    assert_eq!(host.held_count(), 0);
}
#[test]
fn uncertain_release_stays_tracked_through_failed_cleanup() {
    let (mut host, c) = setup();
    let clock = Cell::new(1);
    let mut sink = Sink::new(&clock);
    sink.fault = Some((2, Submission::Unknown));
    let r = receipt(host.dispatch(request(c, 0, LINE), &mut sink, || at(1)));
    assert_eq!(r.outcome, InputOutcome::EffectUnknown);
    assert_eq!(r.submitted_operations, 2);
    assert_eq!(host.held_count(), 1);
    assert_eq!(host.cleanup(&mut sink).remaining, 1);
    assert!(sink.prepared.is_none());
    sink.fault = None;
    assert_eq!(host.cleanup(&mut sink).remaining, 0);
    assert_eq!(sink.ops.len(), 3);
}
#[test]
fn atomic_platform_retains_exact_fractional_and_pixel_scroll_semantics() {
    for unit in [ScrollUnit::Lines, ScrollUnit::Pixels] {
        let (mut host, c) = setup();
        let clock = Cell::new(1);
        let mut sink = Sink::new(&clock);
        sink.pairs = false;
        let mut req = request(c, LINE / 2, -LINE / 4);
        if let InputEvent::Scroll { unit: u, .. } = &mut req.event {
            *u = unit;
        }
        let r = receipt(host.dispatch(req, &mut sink, || at(1)));
        assert_eq!(r.submitted_operations, 2);
        assert_eq!(
            sink.ops[1],
            Operation::Scroll {
                x: LINE / 2,
                y: -LINE / 4,
                unit
            }
        );
    }
}
