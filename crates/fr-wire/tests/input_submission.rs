use fr_core::{
    authority::{AuthorityError, AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::{InputOutcome, InputSequenceError},
    input_submission::*,
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use fr_wire::input::{
    InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, decode_input, encode_input,
};
use fr_wire::input_result::{
    INPUT_RESULT_BYTES, InputResult, ResultBinding, SequenceSpace, Stage, decode_input_result,
    encode_input_result,
};
use std::{
    cell::Cell,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
};
fn time(n: u64) -> HostInstant {
    HostInstant::from_micros(n)
}
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn all() -> Capabilities {
    [
        Capability::Keys,
        Capability::Repeat,
        Capability::Absolute,
        Capability::Buttons,
        Capability::Relative,
        Capability::PixelScroll,
        Capability::LineScroll,
        Capability::Text,
    ]
    .into_iter()
    .fold(Capabilities::default(), Capabilities::with)
}
fn authority() -> SessionAuthority {
    let c = credentials();
    let mut a = SessionAuthority::new(
        c.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(3000),
            ticket_lifetime: HostDuration::from_micros(1000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(time(0)).unwrap();
    a.mark_view_ready(time(0)).unwrap();
    a.grant_lease(c.lease, time(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, time(0)).unwrap();
    a
}
fn owner_with(caps: Capabilities) -> InputSession {
    InputSession::new(
        authority(),
        credentials(),
        InputBounds::new(DesktopPoint { x: -320, y: 0 }, 640, 240).unwrap(),
        caps,
        time(0),
    )
    .unwrap()
}
fn owner() -> InputSession {
    owner_with(all())
}
#[derive(Default)]
struct Sink {
    calls: Vec<Operation>,
    failure: Option<(usize, Submission)>,
    panic_at: Option<usize>,
    before: Option<Box<dyn FnMut()>>,
    after: Option<Box<dyn FnMut()>>,
}
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        if let Some(f) = &mut self.before {
            f();
        }
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.calls.push(op);
        if let Some(f) = &mut self.after {
            f();
        }
        assert_ne!(
            self.panic_at,
            Some(self.calls.len()),
            "deliberate native-boundary panic"
        );
        self.failure
            .filter(|(n, _)| *n == self.calls.len())
            .map_or(Submission::Submitted, |(_, r)| r)
    }
}
fn dispatch(
    o: &mut InputSession,
    s: &mut Sink,
    c: InputCredentials,
    seq: u64,
    event: InputEvent<'_>,
    clock: impl FnMut() -> HostInstant,
) -> Result<Dispatch, Refusal> {
    // Exercise the production byte codec, not a parallel event-only path.
    let request = InputRequest {
        credentials: c,
        sequence: seq,
        event,
    };
    let mut bytes = [0; MAX_INPUT_RECORD_BYTES];
    let route = if event.is_pointer() {
        InputDelivery::Datagram
    } else {
        InputDelivery::Reliable
    };
    let n = encode_input(
        request,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        route,
    )
    .unwrap();
    let parsed = decode_input(
        &bytes[..n],
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        route,
    )
    .unwrap();
    let dispatched = o.dispatch(parsed, s, clock)?;
    if let Dispatch::Completed(receipt) = dispatched {
        let binding = ResultBinding {
            channel: 7,
            session: c.session,
            lease: c.lease,
        };
        let space = if event.is_pointer() {
            SequenceSpace::Pointer
        } else {
            SequenceSpace::Action
        };
        let result = InputResult::from_receipt(binding, space, receipt).unwrap();
        let mut response = [0; INPUT_RESULT_BYTES];
        let len = encode_input_result(
            result,
            &mut response,
            &ProtocolLimits::ABSOLUTE,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        let received = decode_input_result(
            &response[..len],
            &ProtocolLimits::ABSOLUTE,
            binding,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert_eq!(received, result);
        assert_eq!(received.outcome, receipt.outcome);
        assert_eq!(received.submitted_operations, receipt.submitted_operations);
        assert_ne!(received.stage, Stage::Observed);
    }
    Ok(dispatched)
}
fn send(o: &mut InputSession, s: &mut Sink, seq: u64, event: InputEvent<'_>) -> Receipt {
    receipt(dispatch(o, s, credentials(), seq, event, || time(10)).unwrap())
}
fn receipt(d: Dispatch) -> Receipt {
    let Dispatch::Completed(r) = d else {
        panic!("missing receipt")
    };
    r
}
fn key(transition: KeyTransition) -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition,
    }
}
fn button(pressed: bool, barrier: u64) -> InputEvent<'static> {
    InputEvent::Button {
        button: PointerButton::Primary,
        pressed,
        position: DesktopPoint { x: -10, y: 20 },
        barrier,
    }
}

#[test]
fn presses_repeats_releases_and_replays_use_one_owner() {
    let (mut o, mut s) = (owner(), Sink::default());
    assert_eq!(
        send(&mut o, &mut s, 0, key(KeyTransition::Press)).outcome,
        InputOutcome::SubmittedToOs
    );
    assert_eq!(o.held_count(), 1);
    let first = send(&mut o, &mut s, 1, key(KeyTransition::Repeat));
    assert_eq!(send(&mut o, &mut s, 1, key(KeyTransition::Repeat)), first);
    assert_eq!(s.calls.len(), 2);
    send(&mut o, &mut s, 2, key(KeyTransition::Release));
    assert_eq!(o.held_count(), 0);
    assert_eq!(s.calls.len(), 3);
}
#[test]
fn expired_after_preflight_never_reaches_native_submission() {
    let (mut o, mut s) = (owner(), Sink::default());
    let clock = Rc::new(Cell::new(100));
    let moved = clock.clone();
    s.before = Some(Box::new(move || moved.set(1000)));
    let r = receipt(
        dispatch(
            &mut o,
            &mut s,
            credentials(),
            0,
            key(KeyTransition::Press),
            || time(clock.get()),
        )
        .unwrap(),
    );
    assert_eq!(r.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert_eq!(
        r.refusal,
        Some(Refusal::Authority(AuthorityError::TicketExpired))
    );
    assert_eq!(s.calls, [] as [Operation; 0]);
    assert_eq!(o.held_count(), 0);
    assert!(
        o.issue_ticket(InputTicketId::from_raw(4), time(1001))
            .is_err()
    );
}
#[test]
fn position_then_expired_button_reports_the_committed_prefix() {
    let (mut o, mut s) = (owner(), Sink::default());
    let clock = Rc::new(Cell::new(100));
    let moved = clock.clone();
    s.after = Some(Box::new(move || moved.set(1000)));
    let r = receipt(
        dispatch(&mut o, &mut s, credentials(), 0, button(true, 50), || {
            time(clock.get())
        })
        .unwrap(),
    );
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(s.calls.len(), 1);
    assert_eq!(o.held_count(), 0);
    assert_eq!(
        receipt(
            dispatch(&mut o, &mut s, credentials(), 0, button(true, 50), || time(
                1001
            ))
            .unwrap()
        ),
        r
    );
}
#[test]
fn text_is_scalar_bounded_and_unknown_suffix_keeps_confirmed_prefix() {
    let (mut o, mut s) = (owner(), Sink::default());
    s.failure = Some((3, Submission::Unknown));
    let r = send(&mut o, &mut s, 0, InputEvent::Text("a🙂z"));
    assert_eq!(r.outcome, InputOutcome::EffectUnknown);
    assert_eq!(r.submitted_operations, 2);
    assert_eq!(
        s.calls,
        vec![
            Operation::Text('a'),
            Operation::Text('🙂'),
            Operation::Text('z')
        ]
    );
    assert_eq!(send(&mut o, &mut s, 0, InputEvent::Text("a🙂z")), r);
    assert_eq!(s.calls.len(), 3);
    assert!(
        dispatch(
            &mut o,
            &mut s,
            credentials(),
            1,
            InputEvent::Text("retry"),
            || time(11)
        )
        .is_err()
    );
}
#[test]
fn pointer_gaps_do_not_gap_actions_and_click_barriers_never_regress() {
    let (mut o, mut s) = (owner(), Sink::default());
    let motion = InputEvent::Pointer {
        position: DesktopPoint { x: 2, y: 3 },
    };
    send(&mut o, &mut s, 20, motion);
    send(&mut o, &mut s, 0, button(true, 30));
    assert_eq!(
        dispatch(&mut o, &mut s, credentials(), 29, motion, || time(10)),
        Ok(Dispatch::ObsoletePointer)
    );
    send(&mut o, &mut s, 1, button(false, 5));
    assert_eq!(
        dispatch(&mut o, &mut s, credentials(), 30, motion, || time(10)),
        Ok(Dispatch::ObsoletePointer)
    );
    send(&mut o, &mut s, 31, motion);
    assert_eq!(s.calls.len(), 6);
}
#[test]
fn sequence_gaps_fence_pointer_and_release_held_state() {
    let (mut o, mut s) = (owner(), Sink::default());
    send(&mut o, &mut s, 0, key(KeyTransition::Press));
    assert_eq!(
        dispatch(
            &mut o,
            &mut s,
            credentials(),
            2,
            key(KeyTransition::Release),
            || time(10)
        ),
        Err(Refusal::Sequence(InputSequenceError::SequenceGap {
            expected: 1,
            received: 2
        }))
    );
    assert!(
        dispatch(
            &mut o,
            &mut s,
            credentials(),
            90,
            InputEvent::Pointer {
                position: DesktopPoint { x: 0, y: 0 }
            },
            || time(10)
        )
        .is_err()
    );
    assert_eq!(
        o.cleanup(&mut s),
        Cleanup {
            submitted_releases: 1,
            remaining: 0
        }
    );
}
#[test]
fn eviction_never_readmits_and_foreign_credentials_never_consume() {
    let (mut o, mut s) = (owner(), Sink::default());
    let mut c = credentials();
    c.lease = InputLeaseId::from_raw(999);
    assert_eq!(
        dispatch(&mut o, &mut s, c, 0, key(KeyTransition::Press), || time(10)),
        Err(Refusal::StaleLease)
    );
    c = credentials();
    c.session = RemoteSessionId::from_raw(999);
    assert_eq!(
        dispatch(&mut o, &mut s, c, 0, key(KeyTransition::Press), || time(10)),
        Err(Refusal::StaleSession)
    );
    for seq in 0..34 {
        send(&mut o, &mut s, seq, InputEvent::Text("x"));
    }
    assert_eq!(
        dispatch(
            &mut o,
            &mut s,
            credentials(),
            0,
            InputEvent::Text("x"),
            || time(10)
        ),
        Ok(Dispatch::ConsumedWithoutReceipt)
    );
    assert_eq!(s.calls.len(), 34);
}
#[test]
fn every_view_generation_and_off_display_coordinates_refuse() {
    let original = credentials();
    let mut cases = [original; 4];
    cases[0].view.geometry = DisplayGeometryGeneration::from_raw(9);
    cases[1].view.viewport = ViewportMappingGeneration::from_raw(9);
    cases[2].view.configuration = CodecConfigurationGeneration::from_raw(9);
    cases[3].view.recovery = RecoveryGeneration::from_raw(9);
    for c in cases {
        let (mut o, mut s) = (owner(), Sink::default());
        let r = receipt(dispatch(&mut o, &mut s, c, 0, button(true, 0), || time(10)).unwrap());
        assert_eq!(r.refusal, Some(Refusal::StaleView));
        assert_eq!(s.calls, [] as [Operation; 0]);
    }
    let (mut o, mut s) = (owner(), Sink::default());
    let r = send(
        &mut o,
        &mut s,
        0,
        InputEvent::Pointer {
            position: DesktopPoint { x: 320, y: 0 },
        },
    );
    assert_eq!(r.refusal, Some(Refusal::OutOfBounds));
    assert_eq!(s.calls, [] as [Operation; 0]);
}
#[test]
fn independent_revoke_during_preflight_and_between_text_calls() {
    let (mut o, mut s) = (owner(), Sink::default());
    let revoke = o.revoke_handle();
    s.before = Some(Box::new(move || revoke.revoke()));
    let r = send(&mut o, &mut s, 0, InputEvent::Text("secret"));
    assert_eq!(r.outcome, InputOutcome::CancelledBeforeSubmission);
    assert_eq!(s.calls, [] as [Operation; 0]);
    let (mut o, mut s) = (owner(), Sink::default());
    let revoke = o.revoke_handle();
    s.after = Some(Box::new(move || revoke.revoke()));
    let r = send(&mut o, &mut s, 0, InputEvent::Text("abc"));
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(r.submitted_operations, 1);
}
#[test]
fn unknown_press_and_panic_preserve_release_obligations() {
    for panic in [false, true] {
        let (mut o, mut s) = (owner(), Sink::default());
        if panic {
            s.panic_at = Some(1);
        } else {
            s.failure = Some((1, Submission::Unknown));
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            send(&mut o, &mut s, 0, key(KeyTransition::Press))
        }));
        assert_eq!(result.is_err(), panic);
        assert_eq!(o.held_count(), 1);
        let r = send(&mut o, &mut s, 0, key(KeyTransition::Press));
        assert_eq!(r.outcome, InputOutcome::EffectUnknown);
        assert_eq!(s.calls.len(), 1);
        s.panic_at = None;
        s.failure = None;
        assert_eq!(
            o.cleanup(&mut s),
            Cleanup {
                submitted_releases: 1,
                remaining: 0
            }
        );
    }
}
#[test]
fn known_refused_press_does_not_fabricate_held_state() {
    let (mut o, mut s) = (owner(), Sink::default());
    s.failure = Some((1, Submission::NotSubmitted(PlatformError::Permission)));
    let r = send(&mut o, &mut s, 0, key(KeyTransition::Press));
    assert_eq!(r.outcome, InputOutcome::RejectedBeforeSubmission);
    assert_eq!(o.held_count(), 0);
}
#[test]
fn watchdog_expiry_and_failed_cleanup_remain_explicit() {
    let (mut o, mut s) = (owner(), Sink::default());
    send(&mut o, &mut s, 0, key(KeyTransition::Press));
    s.failure = Some((2, Submission::Unknown));
    assert_eq!(
        o.maintain(time(3000), &mut s),
        Cleanup {
            submitted_releases: 0,
            remaining: 1
        }
    );
    assert!(o.revoke_handle().is_revoked());
    assert_eq!(
        o.cleanup(&mut s),
        Cleanup {
            submitted_releases: 1,
            remaining: 0
        }
    );
    assert_eq!(
        o.cleanup(&mut s),
        Cleanup {
            submitted_releases: 0,
            remaining: 0
        }
    );
}
#[test]
fn modes_require_fresh_tickets_and_relative_checkpoints_are_cumulative() {
    let (mut o, mut s) = (owner(), Sink::default());
    let r = send(
        &mut o,
        &mut s,
        0,
        InputEvent::Mode {
            mode: PointerMode::Relative,
            epoch: 1,
        },
    );
    assert_eq!(r.outcome, InputOutcome::AppliedLocally);
    assert_eq!(s.calls, [] as [Operation; 0]);
    let mut c = credentials();
    c.ticket = InputTicketId::from_raw(4);
    o.issue_ticket(c.ticket, time(11)).unwrap();
    for (seq, x, y) in [(1, 10, -4), (2, 12, -5), (3, 12, -5)] {
        let r = receipt(
            dispatch(
                &mut o,
                &mut s,
                c,
                seq,
                InputEvent::Relative {
                    mode_epoch: 1,
                    cumulative_x: x,
                    cumulative_y: y,
                },
                || time(12),
            )
            .unwrap(),
        );
        assert_eq!(
            r.outcome,
            if seq == 3 {
                InputOutcome::AppliedLocally
            } else {
                InputOutcome::SubmittedToOs
            }
        );
    }
    assert_eq!(
        s.calls,
        vec![
            Operation::Relative { x: 10, y: -4 },
            Operation::Relative { x: 2, y: -1 }
        ]
    );
    let r = receipt(
        dispatch(
            &mut o,
            &mut s,
            c,
            4,
            InputEvent::Relative {
                mode_epoch: 1,
                cumulative_x: i64::MIN,
                cumulative_y: 0,
            },
            || time(12),
        )
        .unwrap(),
    );
    assert_eq!(r.refusal, Some(Refusal::RelativeOverflow));
    assert_eq!(s.calls.len(), 2);
}
#[test]
fn mode_changes_do_not_revalidate_old_pointer_packets() {
    let (mut o, mut s) = (owner(), Sink::default());
    send(
        &mut o,
        &mut s,
        0,
        InputEvent::Mode {
            mode: PointerMode::Absolute,
            epoch: 1,
        },
    );
    let r = send(
        &mut o,
        &mut s,
        99,
        InputEvent::Pointer {
            position: DesktopPoint { x: 1, y: 1 },
        },
    );
    assert_eq!(
        r.refusal,
        Some(Refusal::Authority(AuthorityError::TicketInvalid))
    );
    assert_eq!(s.calls, [] as [Operation; 0]);
}
#[test]
fn unsupported_text_repeat_and_view_only_never_inject() {
    let (mut o, mut s) = (owner_with(Capabilities::default()), Sink::default());
    assert_eq!(
        send(&mut o, &mut s, 0, InputEvent::Text("x")).refusal,
        Some(Refusal::Unsupported)
    );
    assert_eq!(s.calls, [] as [Operation; 0]);
    let (mut o, mut s) = (owner(), Sink::default());
    assert_eq!(
        send(&mut o, &mut s, 0, key(KeyTransition::Repeat)).refusal,
        Some(Refusal::InvalidTransition)
    );
    assert_eq!(s.calls, [] as [Operation; 0]);
    let mut a = authority();
    a.revoke_lease();
    assert!(
        InputSession::new(
            a,
            credentials(),
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1, 1).unwrap(),
            all(),
            time(1)
        )
        .is_err()
    );
}
#[test]
fn suspend_focus_loss_and_clock_regression_are_terminal() {
    for reason in 0..3 {
        let (mut o, mut s) = (owner(), Sink::default());
        send(&mut o, &mut s, 0, key(KeyTransition::Press));
        match reason {
            0 => o.suspend(),
            1 => o.invalidate_view(),
            _ => {}
        }
        let result = dispatch(
            &mut o,
            &mut s,
            credentials(),
            1,
            key(KeyTransition::Release),
            || time(1),
        );
        if reason < 2 {
            assert_eq!(result, Err(Refusal::Sequence(InputSequenceError::Fenced)));
        } else {
            let r = receipt(result.unwrap());
            assert_eq!(
                r.refusal,
                Some(Refusal::Authority(AuthorityError::ClockRegression))
            );
            assert_eq!(r.submitted_operations, 0);
        }
        assert_eq!(s.calls.len(), 1);
        assert_eq!(o.cleanup(&mut s).remaining, 0);
    }
}
