use fr_client::input::*;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_wire::{input::*, input_result::*};
const ORIGIN: u64 = 1_000_000_000;
fn local(n: u64) -> ClientInstant {
    ClientInstant(ORIGIN + n)
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
fn caps() -> Capabilities {
    Capabilities::default()
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Text)
        .with(Capability::LineScroll)
}
fn bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: -20, y: -30 }, 340, 270).unwrap()
}
fn client(policy: Policy) -> InputClient {
    InputClient::new(
        credentials(),
        7,
        bounds(),
        caps(),
        ProtocolLimits::ABSOLUTE,
        policy,
        local(0),
    )
    .unwrap()
}
fn evidence(serial: u64, received: u64, age: u64) -> PresentedObservation {
    PresentedObservation {
        session: credentials().session,
        serial,
        view: credentials().view,
        received_at: local(received),
        source_age_upper_us: age,
    }
}
fn ready(viewer: &mut InputClient) {
    viewer
        .confirm_mapping(credentials().session, credentials().view, local(0))
        .unwrap();
    viewer.presented(evidence(0, 0, 0), local(0)).unwrap();
}
fn host() -> InputSession {
    let viewer = credentials();
    let mut a = SessionAuthority::new(viewer.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(HostInstant::ORIGIN).unwrap();
    a.mark_view_ready(HostInstant::ORIGIN).unwrap();
    a.grant_lease(viewer.lease, HostInstant::ORIGIN).unwrap();
    a.issue_input_ticket(viewer.lease, viewer.ticket, HostInstant::ORIGIN)
        .unwrap();
    InputSession::new(a, viewer, bounds(), caps(), HostInstant::ORIGIN).unwrap()
}
#[derive(Default)]
struct Sink {
    operations: Vec<Operation>,
    unknown_at: Option<usize>,
}
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        if self.unknown_at == Some(self.operations.len()) {
            return Submission::Unknown;
        }
        self.operations.push(op);
        Submission::Submitted
    }
}
fn binding() -> ResultBinding {
    ResultBinding {
        channel: 7,
        session: credentials().session,
        lease: credentials().lease,
    }
}
fn result_bytes(result: InputResult) -> Vec<u8> {
    let mut out = vec![0; INPUT_RESULT_BYTES];
    let n = encode_input_result(
        result,
        &mut out,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    out.truncate(n);
    out
}
fn dispatch(h: &mut InputSession, s: &mut Sink, bytes: &[u8], space: SequenceSpace) -> InputResult {
    let delivery = if space == SequenceSpace::Pointer {
        InputDelivery::Datagram
    } else {
        InputDelivery::Reliable
    };
    let request = decode_input(
        bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        delivery,
    )
    .unwrap();
    let Dispatch::Completed(r) = h.dispatch(request, s, || HostInstant::ORIGIN).unwrap() else {
        panic!("terminal receipt required")
    };
    InputResult::from_receipt(binding(), space, r).unwrap()
}
fn action(viewer: &mut InputClient, a: Action<'_>) -> Vec<u8> {
    let mut out = vec![0; MAX_INPUT_RECORD_BYTES];
    let e = viewer.action(a, &mut out, local(1)).unwrap();
    out.truncate(e.bytes);
    out
}
fn pointer(viewer: &mut InputClient, x: i32) -> Vec<u8> {
    let mut out = vec![0; MAX_INPUT_RECORD_BYTES];
    let e = viewer
        .pointer(DesktopPoint { x, y: 1 }, &mut out, local(1))
        .unwrap();
    out.truncate(e.bytes);
    out
}
#[test]
fn grant_ticket_heartbeat_and_decode_do_not_replace_mapping_and_presentation() {
    let mut viewer = client(Policy::default());
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    viewer.ticket(InputTicketId::from_raw(9), local(0)).unwrap();
    viewer.tick(local(0)).unwrap();
    assert_eq!(
        viewer.action(Action::Text("a"), &mut out, local(0)),
        Err(Error::MappingUnconfirmed)
    );
    viewer
        .confirm_mapping(credentials().session, credentials().view, local(0))
        .unwrap();
    assert_eq!(
        viewer.action(Action::Text("a"), &mut out, local(0)),
        Err(Error::NoPresentedView)
    );
    viewer.presented(evidence(0, 0, 0), local(0)).unwrap();
    assert_eq!(
        viewer
            .action(Action::Text("a"), &mut out, local(0))
            .unwrap()
            .sequence,
        0
    );
}
#[test]
fn presentation_age_includes_network_uncertainty_and_time_waiting_to_present() {
    let p = Policy {
        view_age_us: 100,
        receipt_timeout_us: 200,
    };
    let mut viewer = client(p);
    assert_eq!(
        viewer.presented(evidence(0, 0, 60), local(40)),
        Err(Error::Stopped(StopReason::ViewStale))
    );
    let mut viewer = client(p);
    viewer.presented(evidence(1, 0, 60), local(39)).unwrap();
    assert_eq!(
        viewer.presented(evidence(1, 39, 0), local(39)),
        Err(Error::ObsoleteObservation)
    );
    assert_eq!(
        viewer.tick(local(40)),
        Err(Error::Stopped(StopReason::ViewStale))
    );
    assert!(viewer.presented(evidence(2, 40, 0), local(40)).is_err());
    assert!(
        viewer
            .ticket(InputTicketId::from_raw(10), local(40))
            .is_err()
    );
}
#[test]
fn qualified_unchanged_source_can_refresh_static_pixels_without_a_new_frame() {
    let mut viewer = client(Policy {
        view_age_us: 100,
        receipt_timeout_us: 200,
    });
    ready(&mut viewer);
    viewer.presented(evidence(1, 80, 10), local(85)).unwrap();
    viewer.tick(local(160)).unwrap();
    assert_eq!(
        viewer.tick(local(170)),
        Err(Error::Stopped(StopReason::ViewStale))
    );
}
#[test]
fn old_session_and_old_local_observation_never_enable_a_new_grant() {
    let mut viewer = client(Policy::default());
    let mut old = evidence(0, 0, 0);
    old.session = RemoteSessionId::from_raw(99);
    assert_eq!(viewer.presented(old, local(0)), Err(Error::StaleSession));
    assert_eq!(
        viewer.confirm_mapping(old.session, old.view, local(0)),
        Err(Error::StaleSession)
    );
    old = evidence(0, 0, 0);
    old.received_at = ClientInstant(ORIGIN - 1);
    assert_eq!(
        viewer.presented(old, local(0)),
        Err(Error::ObsoleteObservation)
    );
    ready(&mut viewer);
    assert_eq!(viewer.stopped(), None);
}
#[test]
fn every_view_generation_is_fenced_and_clock_regression_is_terminal() {
    let v = credentials().view;
    for view in [
        InputView {
            geometry: v.geometry.next().unwrap(),
            ..v
        },
        InputView {
            viewport: v.viewport.next().unwrap(),
            ..v
        },
        InputView {
            configuration: v.configuration.next().unwrap(),
            ..v
        },
        InputView {
            recovery: v.recovery.next().unwrap(),
            ..v
        },
    ] {
        let mut viewer = client(Policy::default());
        let mut e = evidence(0, 0, 0);
        e.view = view;
        assert_eq!(
            viewer.presented(e, local(0)),
            Err(Error::Stopped(StopReason::ViewChanged))
        );
    }
    let mut viewer = client(Policy::default());
    ready(&mut viewer);
    viewer.tick(local(2)).unwrap();
    assert_eq!(
        viewer.tick(local(1)),
        Err(Error::Stopped(StopReason::ClockRegression))
    );
}
#[test]
fn actual_codecs_host_barriers_and_separate_sequences_prevent_pointer_rewind() {
    let mut viewer = client(Policy::default());
    ready(&mut viewer);
    let mut h = host();
    let mut s = Sink::default();
    let dropped = pointer(&mut viewer, 1);
    let late = pointer(&mut viewer, 2);
    let click = action(
        &mut viewer,
        Action::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 30, y: 40 },
        },
    );
    let req = decode_input(
        &click,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(req.sequence, 0);
    assert!(matches!(req.event, InputEvent::Button { barrier: 2, .. }));
    let r = dispatch(&mut h, &mut s, &click, SequenceSpace::Action);
    assert_eq!(
        viewer.result(&result_bytes(r), local(2)),
        Ok(ResultEvent::Completed(r))
    );
    for old in [dropped, late] {
        let req = decode_input(
            &old,
            &ProtocolLimits::ABSOLUTE,
            7,
            InputDirection::ViewerToHost,
            InputDelivery::Datagram,
        )
        .unwrap();
        assert_eq!(
            h.dispatch(req, &mut s, || HostInstant::ORIGIN),
            Ok(Dispatch::ObsoletePointer)
        );
    }
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let e = viewer
        .pointer(DesktopPoint { x: 50, y: 60 }, &mut out, local(3))
        .unwrap();
    assert_eq!(e.sequence, 3);
    dispatch(&mut h, &mut s, &out[..e.bytes], SequenceSpace::Pointer);
    assert_eq!(
        s.operations,
        vec![
            Operation::Absolute(DesktopPoint { x: 30, y: 40 }),
            Operation::Button {
                button: PointerButton::Primary,
                pressed: true
            },
            Operation::Absolute(DesktopPoint { x: 50, y: 60 })
        ]
    );
}
#[test]
fn physical_keys_committed_unicode_scroll_and_repeats_reach_the_host_separately() {
    let mut viewer = client(Policy::default());
    ready(&mut viewer);
    let mut h = host();
    let mut s = Sink::default();
    let key = PhysicalKey::new(4).unwrap();
    for (n, a) in [
        Action::Key {
            key,
            transition: KeyTransition::Press,
        },
        Action::Key {
            key,
            transition: KeyTransition::Repeat,
        },
        Action::Key {
            key,
            transition: KeyTransition::Release,
        },
        Action::Text("é🦀a"),
        Action::Scroll {
            position: DesktopPoint { x: -10, y: -10 },
            x: 0,
            y: 1,
            unit: ScrollUnit::Lines,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let bytes = action(&mut viewer, a);
        let r = dispatch(&mut h, &mut s, &bytes, SequenceSpace::Action);
        assert_eq!(r.sequence, n as u64);
        assert_eq!(
            viewer.result(&result_bytes(r), local(1)),
            Ok(ResultEvent::Completed(r))
        );
    }
    assert_eq!(s.operations.len(), 8);
    assert_eq!(
        s.operations[3..6],
        [
            Operation::Text('é'),
            Operation::Text('🦀'),
            Operation::Text('a')
        ]
    );
    assert_eq!(viewer.pending_actions(), 0);
    assert_eq!(h.held_count(), 0);
}
#[test]
fn uncertain_unicode_prefix_survives_close_and_cannot_be_reissued_with_a_new_ticket() {
    let mut viewer = client(Policy::default());
    ready(&mut viewer);
    let mut h = host();
    let mut s = Sink {
        unknown_at: Some(2),
        ..Sink::default()
    };
    let bytes = action(&mut viewer, Action::Text("é🦀x"));
    let r = dispatch(&mut h, &mut s, &bytes, SequenceSpace::Action);
    assert_eq!(r.outcome, InputOutcome::EffectUnknown);
    assert_eq!(r.submitted_operations, 2);
    viewer.stop(StopReason::Disconnected);
    assert_eq!(
        viewer.result(&result_bytes(r), local(2)),
        Ok(ResultEvent::Completed(r))
    );
    assert_eq!(viewer.stopped(), Some(StopReason::Disconnected));
    assert!(viewer.ticket(InputTicketId::from_raw(9), local(2)).is_err());
    assert!(
        viewer
            .action(
                Action::Text("é🦀x"),
                &mut [0; MAX_INPUT_RECORD_BYTES],
                local(2)
            )
            .is_err()
    );
    assert_eq!(s.operations, [Operation::Text('é'), Operation::Text('🦀')]);
}
#[test]
fn receipt_window_is_bounded_and_duplicates_cannot_free_new_pending_actions() {
    let mut viewer = client(Policy::default());
    ready(&mut viewer);
    let mut h = host();
    let mut s = Sink::default();
    let mut results = vec![];
    for _ in 0..MAX_PENDING_ACTIONS {
        let b = action(&mut viewer, Action::Text("x"));
        results.push(dispatch(&mut h, &mut s, &b, SequenceSpace::Action));
    }
    assert_eq!(viewer.pending_actions(), 32);
    assert_eq!(
        viewer.action(
            Action::Text("x"),
            &mut [0; MAX_INPUT_RECORD_BYTES],
            local(1)
        ),
        Err(Error::Backpressure)
    );
    assert_eq!(
        viewer.result(&result_bytes(results[0]), local(1)),
        Ok(ResultEvent::Completed(results[0]))
    );
    let next = action(&mut viewer, Action::Text("x"));
    let next = dispatch(&mut h, &mut s, &next, SequenceSpace::Action);
    assert_eq!(next.sequence, 32);
    assert_eq!(
        viewer.result(&result_bytes(results[0]), local(1)),
        Ok(ResultEvent::Duplicate(results[0]))
    );
    assert_eq!(viewer.pending_actions(), 32);
    // Results may arrive out of order without freeing an unrelated slot.
    for r in results[1..].iter().rev() {
        assert_eq!(
            viewer.result(&result_bytes(*r), local(1)),
            Ok(ResultEvent::Completed(*r))
        );
    }
    assert_eq!(
        viewer.result(&result_bytes(next), local(1)),
        Ok(ResultEvent::Completed(next))
    );
    assert_eq!(
        viewer.result(&result_bytes(results[0]), local(1)),
        Ok(ResultEvent::Unretained)
    );
    assert_eq!(viewer.pending_actions(), 0);
}
#[test]
fn receipt_timeout_does_not_erase_a_late_confirmed_effect() {
    let mut viewer = client(Policy {
        view_age_us: 100,
        receipt_timeout_us: 10,
    });
    ready(&mut viewer);
    let mut h = host();
    let mut s = Sink::default();
    let bytes = action(&mut viewer, Action::Text("é"));
    let r = dispatch(&mut h, &mut s, &bytes, SequenceSpace::Action);
    assert_eq!(
        viewer.tick(local(11)),
        Err(Error::Stopped(StopReason::ReceiptTimeout))
    );
    assert_eq!(
        viewer.result(&result_bytes(r), local(12)),
        Ok(ResultEvent::Completed(r))
    );
    assert_eq!(viewer.stopped(), Some(StopReason::ReceiptTimeout));
}
#[test]
fn pointer_results_do_not_ack_actions_and_failed_pointer_results_stop_control() {
    let mut viewer = client(Policy::default());
    ready(&mut viewer);
    let mut h = host();
    let mut s = Sink::default();
    let p = pointer(&mut viewer, 1);
    let _action = action(&mut viewer, Action::Text("x"));
    let r = dispatch(&mut h, &mut s, &p, SequenceSpace::Pointer);
    assert_eq!(r.sequence, 0);
    assert_eq!(
        viewer.result(&result_bytes(r), local(1)),
        Ok(ResultEvent::Pointer(r))
    );
    assert_eq!(viewer.pending_actions(), 1);
    let failed = InputResult {
        outcome: InputOutcome::RejectedBeforeSubmission,
        stage: Stage::Admitted,
        submitted_operations: 0,
        reason: Some(Reason::ViewUnready),
        ..r
    };
    assert_eq!(
        viewer.result(&result_bytes(failed), local(1)),
        Ok(ResultEvent::Pointer(failed))
    );
    assert_eq!(viewer.stopped(), Some(StopReason::ActionFailed));
}
#[test]
fn foreign_future_conflicting_and_action_specific_invalid_results_do_not_free_slots() {
    let mut viewer = client(Policy::default());
    ready(&mut viewer);
    let mut h = host();
    let mut s = Sink::default();
    let bytes = action(&mut viewer, Action::Text("é"));
    let r = dispatch(&mut h, &mut s, &bytes, SequenceSpace::Action);
    let foreign = InputResult {
        binding: ResultBinding {
            lease: InputLeaseId::from_raw(99),
            ..binding()
        },
        ..r
    };
    assert!(viewer.result(&result_bytes(foreign), local(1)).is_err());
    assert_eq!(viewer.stopped(), None);
    assert_eq!(viewer.pending_actions(), 1);
    // Two bytes is only ONE Unicode scalar: generic wire validity is insufficient.
    let impossible = InputResult {
        submitted_operations: 2,
        ..r
    };
    assert_eq!(
        viewer.result(&result_bytes(impossible), local(1)),
        Err(Error::Stopped(StopReason::InvalidReceipt))
    );
    assert_eq!(viewer.pending_actions(), 1);
    assert_eq!(
        viewer.result(&result_bytes(r), local(1)),
        Ok(ResultEvent::Completed(r))
    );
    let conflicting = InputResult {
        stage: Stage::Observed,
        ..r
    };
    assert!(viewer.result(&result_bytes(conflicting), local(1)).is_err());
    let mut viewer = client(Policy::default());
    ready(&mut viewer);
    assert_eq!(
        viewer.result(&result_bytes(r), local(1)),
        Err(Error::Stopped(StopReason::InvalidReceipt))
    );
}
#[test]
fn encoding_failures_leave_counters_and_key_state_unconsumed() {
    let mut viewer = client(Policy::default());
    ready(&mut viewer);
    let key = PhysicalKey::new(4).unwrap();
    let a = || Action::Key {
        key,
        transition: KeyTransition::Press,
    };
    assert!(viewer.action(a(), &mut [0; 10], local(1)).is_err());
    assert_eq!(viewer.pending_actions(), 0);
    let b = action(&mut viewer, a());
    let req = decode_input(
        &b,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(req.sequence, 0);
    assert_eq!(
        viewer.action(a(), &mut [0; MAX_INPUT_RECORD_BYTES], local(1)),
        Err(Error::InvalidTransition)
    );
    assert_eq!(
        viewer.pointer(
            DesktopPoint { x: 320, y: 0 },
            &mut [0; MAX_INPUT_RECORD_BYTES],
            local(1)
        ),
        Err(Error::OutOfBounds)
    );
    assert!(
        viewer
            .pointer(DesktopPoint { x: 1, y: 0 }, &mut [0; 1], local(1))
            .is_err()
    );
    let p = pointer(&mut viewer, 1);
    let r = decode_input(
        &p,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Datagram,
    )
    .unwrap();
    assert_eq!(r.sequence, 0);
}
#[test]
fn focus_suspend_disconnect_and_view_change_are_sticky_terminal_boundaries() {
    for reason in [
        StopReason::FocusLost,
        StopReason::Suspended,
        StopReason::Disconnected,
        StopReason::ViewChanged,
    ] {
        let mut viewer = client(Policy::default());
        ready(&mut viewer);
        viewer.stop(reason);
        viewer.stop(StopReason::Disconnected);
        assert_eq!(viewer.stopped(), Some(reason));
        assert!(viewer.presented(evidence(1, 1, 0), local(1)).is_err());
        assert!(
            viewer
                .action(
                    Action::Text("x"),
                    &mut [0; MAX_INPUT_RECORD_BYTES],
                    local(1)
                )
                .is_err()
        );
    }
}
