//! The sink's rules over a recording stand-in for Windows. These prove the
//! decisions and the held-state bookkeeping, not that Windows acts on them
//! (that is the live check on a Windows host).
use super::*;
use fr_core::input::PhysicalKey;
use std::{cell::RefCell, rc::Rc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Key(Key, bool),
    Text(char),
    Move(DesktopPoint),
    Button(PointerButton, bool),
    Scroll(i32, i32),
}

struct Fake {
    displays: Vec<InputBounds>,
    cursor: DesktopPoint,
    default_desktop: bool,
    elevated_foreground: bool,
    result: Sent,
    calls: Rc<RefCell<Vec<Call>>>,
    move_preparation: Option<Box<dyn FnMut() -> Result<(), PlatformError>>>,
}
impl Native for Fake {
    type PreparedMove = DesktopPoint;

    fn displays(&mut self) -> Result<Vec<InputBounds>, PlatformError> {
        Ok(self.displays.clone())
    }
    fn cursor(&mut self) -> Result<DesktopPoint, PlatformError> {
        Ok(self.cursor)
    }
    fn desktop_is_default(&mut self) -> bool {
        self.default_desktop
    }
    fn foreground_outranks_us(&mut self) -> bool {
        self.elevated_foreground
    }
    fn key(&mut self, key: Key, down: bool) -> Sent {
        self.record(Call::Key(key, down))
    }
    fn text(&mut self, scalar: char) -> Sent {
        self.record(Call::Text(scalar))
    }
    fn prepare_move_to(&mut self, point: DesktopPoint) -> Result<DesktopPoint, PlatformError> {
        if let Some(prepare) = &mut self.move_preparation {
            prepare()?;
        }
        Ok(point)
    }
    fn move_to(&mut self, prepared: DesktopPoint) -> Sent {
        self.record(Call::Move(prepared))
    }
    fn button(&mut self, button: PointerButton, down: bool) -> Sent {
        self.record(Call::Button(button, down))
    }
    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Sent {
        self.record(Call::Scroll(horizontal, vertical))
    }
}
impl Fake {
    fn record(&mut self, call: Call) -> Sent {
        self.calls.borrow_mut().push(call);
        self.result
    }
}

const fn point(x: i32, y: i32) -> DesktopPoint {
    DesktopPoint { x, y }
}
fn bounds(x: i32, y: i32, width: u32, height: u32) -> InputBounds {
    InputBounds::new(point(x, y), width, height).unwrap()
}
/// oldsurface's layout: a 2256x1504 panel, plus a display to its left.
fn main_display() -> InputBounds {
    bounds(0, 0, 2256, 1504)
}
fn left_display() -> InputBounds {
    bounds(-1920, 200, 1920, 1080)
}

struct Harness {
    sink: WindowsInput<Fake>,
    calls: Rc<RefCell<Vec<Call>>>,
}
impl Harness {
    fn new() -> Self {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let fake = Fake {
            displays: vec![left_display(), main_display()],
            cursor: point(100, 100),
            default_desktop: true,
            elevated_foreground: false,
            result: Sent::All,
            calls: calls.clone(),
            move_preparation: None,
        };
        Self {
            sink: WindowsInput::open(fake, main_display(), implemented()).unwrap(),
            calls,
        }
    }
    /// The owner's order: prepare, then submit (its final check between).
    fn run(&mut self, op: Operation) -> Result<Submission, PlatformError> {
        self.sink.prepare(op)?;
        Ok(self.sink.submit(op))
    }
    fn taken(&self) -> Vec<Call> {
        self.calls.borrow_mut().drain(..).collect()
    }
    fn fake(&mut self) -> &mut Fake {
        &mut self.sink.native
    }
}

fn key(usage: u16, transition: KeyTransition) -> Operation {
    Operation::Key {
        key: PhysicalKey::new(usage).unwrap(),
        transition,
    }
}
const fn button(pressed: bool) -> Operation {
    Operation::Button {
        button: PointerButton::Primary,
        pressed,
    }
}
const A: u16 = 0x04;

#[test]
fn a_lease_binds_only_an_exact_display_and_implemented_operations() {
    let h = Harness::new();
    let open = |b: InputBounds, caps: Capabilities| {
        WindowsInput::open(
            Fake {
                displays: vec![left_display(), main_display()],
                cursor: point(0, 0),
                default_desktop: true,
                elevated_foreground: false,
                result: Sent::All,
                calls: h.calls.clone(),
                move_preparation: None,
            },
            b,
            caps,
        )
        .map(drop)
    };
    assert_eq!(open(left_display(), implemented()), Ok(()));
    // One pixel off is another display.
    assert_eq!(
        open(bounds(0, 0, 2256, 1503), implemented()),
        Err(PlatformError::GeometryChanged)
    );
    for missing in [Capability::Relative, Capability::PixelScroll] {
        assert_eq!(
            open(main_display(), implemented().with(missing)),
            Err(PlatformError::Unsupported)
        );
    }
    assert_eq!(h.taken(), []);
}

#[test]
fn keys_go_by_scan_code_with_pause_and_num_lock_by_virtual_key() {
    let mut h = Harness::new();
    for (usage, native) in [
        (
            A,
            Key::Scan {
                code: 0x1E,
                extended: false,
            },
        ),
        (
            0xE4,
            Key::Scan {
                code: 0x1D,
                extended: true,
            },
        ), // right Ctrl
        (
            0x52,
            Key::Scan {
                code: 0x48,
                extended: true,
            },
        ), // Up
        (
            0x60,
            Key::Scan {
                code: 0x48,
                extended: false,
            },
        ), // keypad 8
        (0x48, Key::Pause),
        (0x53, Key::NumLock),
    ] {
        assert_eq!(
            h.run(key(usage, KeyTransition::Press)),
            Ok(Submission::Submitted)
        );
        assert_eq!(
            h.run(key(usage, KeyTransition::Release)),
            Ok(Submission::Submitted)
        );
        assert_eq!(
            h.taken(),
            [Call::Key(native, true), Call::Key(native, false)]
        );
    }
    // A usage no Windows key produces (F13) is refused, never guessed.
    assert_eq!(
        h.run(key(0x68, KeyTransition::Press)),
        Err(PlatformError::Unsupported)
    );
    assert_eq!(h.taken(), []);
}

#[test]
fn presses_releases_and_repeats_follow_what_is_held() {
    let mut h = Harness::new();
    let a = Key::Scan {
        code: 0x1E,
        extended: false,
    };
    assert_eq!(
        h.run(key(A, KeyTransition::Release)),
        Err(PlatformError::Unsupported)
    );
    assert_eq!(
        h.run(key(A, KeyTransition::Repeat)),
        Err(PlatformError::Unsupported)
    );
    assert_eq!(
        h.run(key(A, KeyTransition::Press)),
        Ok(Submission::Submitted)
    );
    assert_eq!(
        h.run(key(A, KeyTransition::Press)),
        Err(PlatformError::Permission)
    );
    // A repeat is another key-down; modifiers repeat on Windows too.
    assert_eq!(
        h.run(key(A, KeyTransition::Repeat)),
        Ok(Submission::Submitted)
    );
    assert_eq!(
        h.run(key(0xE1, KeyTransition::Press)),
        Ok(Submission::Submitted)
    );
    assert_eq!(
        h.run(key(0xE1, KeyTransition::Repeat)),
        Ok(Submission::Submitted)
    );
    let shift = Key::Scan {
        code: 0x2A,
        extended: false,
    };
    assert_eq!(
        h.taken(),
        [
            Call::Key(a, true),
            Call::Key(a, true),
            Call::Key(shift, true),
            Call::Key(shift, true)
        ]
    );
    // Text never turns into a shortcut while keys are held.
    assert_eq!(h.run(Operation::Text('é')), Err(PlatformError::Permission));
    assert!(h.sink.native_cleanup());
    assert_eq!(h.taken(), [Call::Key(a, false), Call::Key(shift, false)]);
    assert_eq!(h.run(Operation::Text('é')), Ok(Submission::Submitted));
    assert_eq!(h.run(Operation::Text('😀')), Ok(Submission::Submitted));
    assert_eq!(h.taken(), [Call::Text('é'), Call::Text('😀')]);
}

#[test]
fn pointer_moves_stay_on_the_bound_display_and_presses_act_where_it_is() {
    let mut h = Harness::new();
    // The real pointer is on the left display: a press there is refused.
    h.fake().cursor = point(-50, 300);
    assert_eq!(h.run(button(true)), Err(PlatformError::GeometryChanged));
    // Off the bound display (left display, one past the edge): refused.
    for outside in [point(-1, 10), point(2256, 10), point(10, 1504)] {
        assert_eq!(
            h.run(Operation::Absolute(outside)),
            Err(PlatformError::GeometryChanged)
        );
    }
    assert_eq!(h.taken(), []);
    let corner = point(2255, 1503);
    assert_eq!(
        h.run(Operation::Absolute(corner)),
        Ok(Submission::Submitted)
    );
    // Presses now act where this owner put the pointer, not the stale cursor.
    assert_eq!(h.run(button(true)), Ok(Submission::Submitted));
    assert_eq!(h.run(button(true)), Err(PlatformError::Permission));
    assert_eq!(h.run(button(false)), Ok(Submission::Submitted));
    assert_eq!(h.run(button(false)), Err(PlatformError::Unsupported));
    assert_eq!(
        h.taken(),
        [
            Call::Move(corner),
            Call::Button(PointerButton::Primary, true),
            Call::Button(PointerButton::Primary, false)
        ]
    );
}

#[test]
fn wheel_lines_turn_by_whole_notches_in_windows_directions() {
    let mut h = Harness::new();
    let scroll = |x: i32, y: i32, unit| Operation::Scroll { x, y, unit };
    // Two lines right, three down (toward the user).
    assert_eq!(
        h.run(scroll(2 * LINE, 3 * LINE, ScrollUnit::Lines)),
        Ok(Submission::Submitted)
    );
    assert_eq!(
        h.run(scroll(-LINE, -LINE, ScrollUnit::Lines)),
        Ok(Submission::Submitted)
    );
    assert_eq!(
        h.taken(),
        [Call::Scroll(240, -360), Call::Scroll(-120, 120)]
    );
    for refused in [
        scroll(LINE / 2, 0, ScrollUnit::Lines),
        scroll(0, 0, ScrollUnit::Lines),
        scroll(0, 33 * LINE, ScrollUnit::Lines),
        scroll(0, 10, ScrollUnit::Pixels),
        Operation::Relative { x: 1, y: 1 },
    ] {
        assert_eq!(h.run(refused), Err(PlatformError::Unsupported));
    }
    assert_eq!(h.taken(), []);
}

#[test]
fn the_lock_screen_and_display_changes_stop_positive_input_but_not_releases() {
    let mut h = Harness::new();
    assert_eq!(
        h.run(key(A, KeyTransition::Press)),
        Ok(Submission::Submitted)
    );
    assert_eq!(h.run(button(true)), Ok(Submission::Submitted));
    h.taken();
    h.fake().default_desktop = false;
    for op in [
        key(0x05, KeyTransition::Press),
        Operation::Absolute(point(5, 5)),
        Operation::Scroll {
            x: 0,
            y: LINE,
            unit: ScrollUnit::Lines,
        },
    ] {
        assert_eq!(h.run(op), Err(PlatformError::Unavailable));
    }
    assert_eq!(
        h.run(key(A, KeyTransition::Release)),
        Ok(Submission::Submitted)
    );
    h.fake().default_desktop = true;
    h.fake().displays = vec![bounds(0, 0, 1920, 1080)];
    assert_eq!(
        h.run(Operation::Absolute(point(5, 5))),
        Err(PlatformError::GeometryChanged)
    );
    assert_eq!(h.run(button(false)), Ok(Submission::Submitted));
    assert_eq!(
        h.taken(),
        [
            Call::Key(
                Key::Scan {
                    code: 0x1E,
                    extended: false
                },
                false
            ),
            Call::Button(PointerButton::Primary, false)
        ]
    );
}

#[test]
fn an_elevated_focused_window_refuses_actions_but_not_moves_or_releases() {
    let mut h = Harness::new();
    assert_eq!(
        h.run(key(A, KeyTransition::Press)),
        Ok(Submission::Submitted)
    );
    h.taken();
    h.fake().elevated_foreground = true;
    for op in [
        key(0x05, KeyTransition::Press),
        key(A, KeyTransition::Repeat),
        button(true),
        Operation::Scroll {
            x: 0,
            y: LINE,
            unit: ScrollUnit::Lines,
        },
    ] {
        assert_eq!(h.run(op), Err(PlatformError::Permission), "{op:?}");
    }
    assert_eq!(
        h.run(Operation::Absolute(point(7, 7))),
        Ok(Submission::Submitted)
    );
    assert_eq!(
        h.run(key(A, KeyTransition::Release)),
        Ok(Submission::Submitted)
    );
    assert_eq!(h.run(Operation::Text('x')), Err(PlatformError::Permission));
    assert_eq!(h.taken().len(), 2);
}

#[test]
fn uncertain_presses_stay_held_for_cleanup_and_failed_ones_do_not() {
    let mut h = Harness::new();
    // Nothing inserted: not submitted, and nothing to release.
    h.fake().result = Sent::Nothing;
    assert_eq!(
        h.run(key(A, KeyTransition::Press)),
        Ok(Submission::NotSubmitted(PlatformError::Unavailable))
    );
    assert!(h.sink.native_cleanup());
    h.taken();
    // Part of a batch: unknown, and the press counts as held.
    h.fake().result = Sent::Partly;
    assert_eq!(h.run(key(A, KeyTransition::Press)), Ok(Submission::Unknown));
    assert_eq!(h.run(button(true)), Ok(Submission::Unknown));
    h.taken();
    // A release that is not confirmed leaves cleanup unresolved...
    assert!(!h.sink.native_cleanup());
    h.taken();
    // ...until one is.
    h.fake().result = Sent::All;
    assert!(h.sink.native_cleanup());
    assert_eq!(h.taken().len(), 2);
    assert!(h.sink.native_cleanup());
    assert_eq!(h.taken(), []);
}

#[test]
fn only_the_prepared_operation_is_sent_and_dropping_releases_everything() {
    let mut h = Harness::new();
    assert_eq!(
        h.sink.submit(Operation::Text('x')),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    h.sink.prepare(Operation::Text('x')).unwrap();
    assert_eq!(
        h.sink.submit(Operation::Text('y')),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    h.sink.prepare(Operation::Text('x')).unwrap();
    h.sink.cancel_prepared();
    assert_eq!(
        h.sink.submit(Operation::Text('x')),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    assert_eq!(h.taken(), []);
    assert_eq!(
        h.run(key(A, KeyTransition::Press)),
        Ok(Submission::Submitted)
    );
    assert_eq!(h.run(button(true)), Ok(Submission::Submitted));
    h.taken();
    let calls = h.calls.clone();
    drop(h);
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn prepared_moves_are_single_use_and_cancelled_on_every_refusal() {
    let mut h = Harness::new();
    let movement = Operation::Absolute(point(30, 40));
    h.sink.prepare(movement).unwrap();
    assert!(h.sink.prepared_move.is_some());
    h.sink.cancel_prepared();
    assert!(h.sink.prepared_move.is_none());
    assert_eq!(
        h.sink.submit(movement),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );

    h.sink.prepare(movement).unwrap();
    assert_eq!(
        h.sink.submit(Operation::Absolute(point(50, 60))),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    assert!(h.sink.prepared_move.is_none());
    assert_eq!(h.taken(), []);

    assert_eq!(h.run(movement), Ok(Submission::Submitted));
    assert!(h.sink.prepared_move.is_none());
    assert_eq!(
        h.sink.submit(movement),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    assert_eq!(h.taken(), [Call::Move(point(30, 40))]);

    h.sink.prepare(movement).unwrap();
    h.fake().move_preparation = Some(Box::new(|| Err(PlatformError::GeometryChanged)));
    assert_eq!(
        h.sink.prepare(movement),
        Err(PlatformError::GeometryChanged)
    );
    assert!(h.sink.prepared_move.is_none());
    assert_eq!(
        h.sink.submit(movement),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    assert_eq!(h.taken(), []);
}

fn input_owner() -> (
    fr_core::input_submission::InputSession,
    fr_core::input::InputCredentials,
) {
    use fr_core::{
        authority::{AuthorityPolicy, SessionAuthority},
        ids::{
            CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
            RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
        },
        input::{InputCredentials, InputView},
        input_submission::InputSession,
        time::HostInstant,
    };

    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(11),
        lease: InputLeaseId::from_raw(12),
        ticket: InputTicketId::from_raw(13),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let now = HostInstant::ORIGIN;
    let mut authority =
        SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(credentials.lease, now).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, now)
        .unwrap();
    let owner =
        InputSession::new(authority, credentials, main_display(), implemented(), now).unwrap();
    (owner, credentials)
}

#[test]
fn native_move_preparation_cannot_spend_the_original_click_or_scroll_deadline() {
    use fr_core::{
        input::{InputEvent, InputRequest},
        input_sequence::InputOutcome,
        input_submission::Dispatch,
        time::HostInstant,
    };
    use std::cell::Cell;

    for event in [
        InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: point(30, 40),
            barrier: 0,
        },
        InputEvent::Scroll {
            x: 0,
            y: LINE,
            unit: ScrollUnit::Lines,
            position: point(30, 40),
            barrier: 0,
        },
    ] {
        for prepared_at in [999_999, 1_000_000, 1_000_001] {
            let (mut owner, credentials) = input_owner();
            let mut h = Harness::new();
            let now = Rc::new(Cell::new(100));
            let prepared_now = now.clone();
            h.fake().move_preparation = Some(Box::new(move || {
                prepared_now.set(prepared_at);
                Ok(())
            }));
            let dispatched = owner
                .dispatch(
                    InputRequest {
                        credentials,
                        sequence: 0,
                        event,
                    },
                    &mut h.sink,
                    || HostInstant::from_micros(now.get()),
                )
                .unwrap();
            let Dispatch::Completed(receipt) = dispatched else {
                panic!("the original request must report its outcome");
            };
            if prepared_at < 1_000_000 {
                assert_eq!(receipt.outcome, InputOutcome::SubmittedToOs);
                assert_eq!(receipt.submitted_operations, 2);
                let action = match event {
                    InputEvent::Button { .. } => Call::Button(PointerButton::Primary, true),
                    _ => Call::Scroll(0, -120),
                };
                assert_eq!(h.taken(), [Call::Move(point(30, 40)), action]);
            } else {
                assert_eq!(receipt.outcome, InputOutcome::ExpiredBeforeSubmission);
                assert_eq!(receipt.submitted_operations, 0);
                assert_eq!(owner.held_count(), 0);
                assert!(owner.monitor().is_revoked());
                assert_eq!(h.taken(), []);
            }
            assert!(h.sink.prepared_move.is_none());
            assert!(h.sink.prepared.is_none());
        }
    }
}

#[test]
fn revoke_during_native_move_preparation_preserves_earlier_held_key_cleanup() {
    use fr_core::{
        input::{InputEvent, InputRequest},
        input_sequence::InputOutcome,
        input_submission::{Dispatch, Refusal},
        time::HostInstant,
    };

    let (mut owner, credentials) = input_owner();
    let mut h = Harness::new();
    let request = |sequence, event| InputRequest {
        credentials,
        sequence,
        event,
    };
    let pressed = owner
        .dispatch(
            request(
                0,
                InputEvent::Key {
                    key: PhysicalKey::new(A).unwrap(),
                    transition: KeyTransition::Press,
                },
            ),
            &mut h.sink,
            || HostInstant::from_micros(100),
        )
        .unwrap();
    assert!(matches!(
        pressed,
        Dispatch::Completed(fr_core::input_submission::Receipt {
            outcome: InputOutcome::SubmittedToOs,
            submitted_operations: 1,
            refusal: None,
            ..
        })
    ));
    assert_eq!(owner.held_count(), 1);
    h.taken();

    let revoke = owner.revoke_handle();
    h.fake().move_preparation = Some(Box::new(move || {
        revoke.revoke();
        Ok(())
    }));
    let dispatched = owner
        .dispatch(
            request(
                1,
                InputEvent::Button {
                    button: PointerButton::Primary,
                    pressed: true,
                    position: point(30, 40),
                    barrier: 0,
                },
            ),
            &mut h.sink,
            || HostInstant::from_micros(200),
        )
        .unwrap();
    let Dispatch::Completed(receipt) = dispatched else {
        panic!("the revoked request must report its outcome");
    };
    assert_eq!(receipt.outcome, InputOutcome::CancelledBeforeSubmission);
    assert_eq!(receipt.refusal, Some(Refusal::Revoked));
    assert_eq!(receipt.submitted_operations, 0);
    assert_eq!(h.taken(), []);
    assert!(h.sink.prepared_move.is_none());

    let cleanup = owner.cleanup(&mut h.sink);
    assert_eq!(cleanup.submitted_releases, 1);
    assert_eq!(cleanup.remaining, 0);
    assert_eq!(
        h.taken(),
        [Call::Key(
            Key::Scan {
                code: 0x1E,
                extended: false
            },
            false
        )]
    );
}
