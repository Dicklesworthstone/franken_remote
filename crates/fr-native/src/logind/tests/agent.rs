//! Real private bus -> real `SessionAgent` fencing; OS input effects are fixtures.
use super::*;
use crate::logind::{
    agent::{Error as EventError, Events},
    input::Gate,
};
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    ids::RemoteSessionId,
    input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton},
    input_submission::{InputSink, Operation, PlatformError, Submission},
    time::HostInstant,
};
use frd::session_agent::{
    ApprovalMode, AudioScope, PeerIdentity, PermissionKind, PermissionStatus, PlatformKind,
    RequestedScope, SessionAgent, SessionRole, source::desktop::LocalAction,
};
fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn agent(id: u32) -> SessionAgent {
    SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxX11,
        id,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
    )
}
fn now(cx: &asupersync::cx::Cx) -> HostInstant {
    frd::input_watchdog::host_now(cx).unwrap()
}
fn input_pair() -> [Operation; 2] {
    [
        Operation::Key {
            key: PhysicalKey::new(0xe1).unwrap(),
            transition: KeyTransition::Press,
        },
        Operation::Button {
            button: PointerButton::Primary,
            pressed: true,
        },
    ]
}
fn grant(agent: &mut SessionAgent, at: HostInstant) {
    agent.approval_mut().set_mode(ApprovalMode::Unattended);
    agent
        .request_session(
            RemoteSessionId::from_raw(12),
            &PeerIdentity {
                node_id: "fixture".into(),
                node_name: "fixture".into(),
                user_id: "fixture".into(),
            },
            &RequestedScope {
                role: SessionRole::Controller,
                displays: vec![0],
                audio: AudioScope::None,
                clipboard: false,
                file_transfer: false,
            },
            at,
        )
        .unwrap();
    for operation in input_pair() {
        agent
            .verify_and_track_submission(RemoteSessionId::from_raw(12), &operation, at)
            .unwrap();
    }
}
#[test]
fn active_evidence_never_creates_approval_capture_permission_or_unlocks() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut watch = start(&peer);
    active(&watch);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut agent = agent(1);
    agent
        .permissions_mut()
        .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Denied);
    let mut events = Events::new(watch.control(), &agent, cx).unwrap();
    assert_eq!(
        events.service(&mut agent, &mut Context::from_waker(Waker::noop())),
        Ok(LocalAction::Continue)
    );
    assert_eq!(
        agent.permissions().status(PermissionKind::ScreenCapture),
        PermissionStatus::Denied
    );
    assert!(!agent.is_observation_admitted(RemoteSessionId::from_raw(12)));
    assert!(!agent.is_control_admitted(RemoteSessionId::from_raw(12)));
    agent.permissions_mut().on_session_locked();
    assert_eq!(
        events.service(&mut agent, &mut Context::from_waker(Waker::noop())),
        Err(EventError::SessionLocked)
    );
    assert!(agent.is_revoked());
    assert!(agent.permissions().is_locked());
    assert!(!events.cleanup().unwrap().outcome.os_cleanup.is_complete());
    finish(&mut watch);
}
#[test]
fn native_lock_fences_authority_and_retains_original_release_batch_and_receipt() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut watch = start(&peer);
    active(&watch);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let broker = rt.request_cx_with_budget(Budget::INFINITE);
    let mut agent = agent(1);
    grant(&mut agent, now(&cx));
    let revoked = Arc::new(AtomicBool::new(false));
    let copied = revoked.clone();
    agent.indicator().register_custom_revoker(move || {
        copied.store(true, Ordering::Release);
    });
    let mut events = Events::new(watch.control(), &agent, cx.clone()).unwrap();
    peer.emit(Event::LockUnlock);
    stopped(&watch, StopReason::Locked);
    let expected = Err(EventError::Evidence(StopReason::Locked));
    assert_eq!(
        events.service(&mut agent, &mut Context::from_waker(Waker::noop())),
        expected
    );
    assert!(revoked.load(Ordering::Acquire));
    assert!(agent.is_revoked() && agent.permissions().is_locked());
    assert!(!agent.is_observation_admitted(RemoteSessionId::from_raw(12)));
    assert!(!agent.is_control_admitted(RemoteSessionId::from_raw(12)));
    assert!(cx.is_cancel_requested());
    assert!(!broker.is_cancel_requested());
    assert_eq!(
        events.cleanup().unwrap().releases,
        [
            Operation::Button {
                button: PointerButton::Primary,
                pressed: false
            },
            Operation::Key {
                key: PhysicalKey::new(0xe1).unwrap(),
                transition: KeyTransition::Release
            }
        ]
    );
    assert!(!agent.held_state().is_clean());
    assert_eq!(
        agent.held_state().last_certainty(),
        frd::session_agent::ReleaseCertainty::PendingCleanup
    );
    let receipt = events.cleanup().unwrap().outcome.os_cleanup.clone();
    assert!(!receipt.is_complete());
    assert_eq!(
        events.service(&mut agent, &mut Context::from_waker(Waker::noop())),
        expected
    );
    let cleanup = events.take_cleanup().unwrap();
    assert_eq!(cleanup.releases.len(), 2);
    assert!(events.take_cleanup().is_none());
    assert_eq!(
        events.service(&mut agent, &mut Context::from_waker(Waker::noop())),
        expected
    );
    assert!(events.take_cleanup().is_none());
    assert!(!receipt.is_complete());
    // Explicit synthetic native cleanup acknowledgement, NOT an OS-effect test.
    for operation in &cleanup.releases {
        agent.held_state_mut().record_injected_operation(operation);
    }
    cleanup.outcome.os_cleanup.mark_complete();
    assert!(receipt.is_complete());
    assert!(agent.held_state().is_clean());
    finish(&mut watch);
}
#[test]
fn suspend_logout_and_lost_service_revoke_without_inventing_screen_lock() {
    let _serial = SERIAL.lock().unwrap();
    for event in [Event::Suspend, Event::Removed, Event::OwnerLost] {
        let peer = Peer::new(Data::default());
        let mut watch = start(&peer);
        active(&watch);
        let rt = runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let mut agent = agent(1);
        grant(&mut agent, now(&cx));
        let mut events = Events::new(watch.control(), &agent, cx).unwrap();
        peer.emit(event);
        wait(|| matches!(watch.control.status(), Status::Stopped(_)));
        let mut callback = events.callback();
        assert_eq!(
            callback(&mut agent, &mut Context::from_waker(Waker::noop())),
            Err(())
        );
        drop(callback);
        assert!(agent.is_revoked());
        assert!(!agent.permissions().is_locked());
        assert_eq!(events.take_cleanup().unwrap().releases.len(), 2);
        finish(&mut watch);
    }
}
#[test]
fn agent_brand_cannot_retarget_another_agent_with_reused_numeric_ids() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut watch = start(&peer);
    active(&watch);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut original = agent(7);
    let mut foreign = agent(7);
    grant(&mut original, now(&cx));
    let mut events = Events::new(watch.control(), &original, cx).unwrap();
    assert_eq!(
        events.service(&mut foreign, &mut Context::from_waker(Waker::noop())),
        Err(EventError::WrongAgent)
    );
    assert!(!foreign.is_revoked());
    assert_eq!(
        events.service(&mut original, &mut Context::from_waker(Waker::noop())),
        Err(EventError::Cancelled)
    );
    assert!(original.is_revoked());
    assert_eq!(events.take_cleanup().unwrap().releases.len(), 2);
    finish(&mut watch);
}
#[test]
fn missing_evidence_and_unsupported_agents_never_attach() {
    let control = Control(Arc::new(Shared {
        selection: selection(),
        state: AtomicU8::new(0),
        deadline: AtomicU64::new(bus::boottime().unwrap() + START_NS),
        waker: Mutex::new(None),
    }));
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let a = agent(1);
    assert!(matches!(
        Events::new(control.clone(), &a, cx.clone()),
        Err(EventError::Opening)
    ));
    control.stop();
    assert!(matches!(
        Events::new(control, &a, cx),
        Err(EventError::Evidence(StopReason::OwnerStopped))
    ));
    assert!(!a.is_revoked());
    let identity = a.identity();
    drop(a);
    assert!(identity.is_revoked());
}
#[derive(Default)]
struct Sink {
    calls: u32,
    cancelled: u32,
    during_prepare: Option<Control>,
    during_submit: Option<Control>,
    unknown: bool,
}
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        if let Some(control) = &self.during_prepare {
            control.stop();
        }
        Ok(())
    }
    fn submit(&mut self, _: Operation) -> Submission {
        self.calls += 1;
        if let Some(control) = &self.during_submit {
            control.stop();
        }
        if self.unknown {
            Submission::Unknown
        } else {
            Submission::Submitted
        }
    }
    fn cancel_prepared(&mut self) {
        self.cancelled += 1;
    }
    fn repeat_requires_pair(&self) -> bool {
        true
    }
    fn line_scroll_requires_pairs(&self) -> bool {
        true
    }
}
#[test]
fn lock_between_prepare_and_os_submit_is_refused_without_input_retry() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut watch = start(&peer);
    active(&watch);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let owner = input_support::session(&cx);
    let mut sink = Gate::new(Sink::default(), watch.control(), owner.monitor());
    let operation = input_pair()[0];
    sink.prepare(operation).unwrap();
    assert!(sink.repeat_requires_pair() && sink.line_scroll_requires_pairs());
    peer.emit(Event::LockUnlock);
    stopped(&watch, StopReason::Locked);
    assert_eq!(
        sink.submit(operation),
        Submission::NotSubmitted(PlatformError::Permission)
    );
    assert_eq!(sink.inner.calls, 0);
    assert_eq!(sink.inner.cancelled, 1);
    assert_eq!(sink.prepare(operation), Err(PlatformError::Permission));
    assert_eq!(sink.inner.calls, 0);
    finish(&mut watch);
}
#[test]
fn preparation_revocation_cleans_and_submission_race_preserves_actual_effect() {
    let _serial = SERIAL.lock().unwrap();
    for case in 0..3 {
        let peer = Peer::new(Data::default());
        let mut watch = start(&peer);
        active(&watch);
        let control = watch.control();
        let inner = Sink {
            during_prepare: (case == 0).then(|| control.clone()),
            during_submit: (case != 0).then(|| control.clone()),
            unknown: case == 2,
            ..Sink::default()
        };
        let rt = runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let owner = input_support::session(&cx);
        let mut sink = Gate::new(inner, control, owner.monitor());
        let operation = input_pair()[0];
        if case == 0 {
            assert_eq!(sink.prepare(operation), Err(PlatformError::Permission));
            assert_eq!(sink.inner.calls, 0);
            assert_eq!(sink.inner.cancelled, 1);
        } else {
            sink.prepare(operation).unwrap();
            assert_eq!(
                sink.submit(operation),
                if case == 2 {
                    Submission::Unknown
                } else {
                    Submission::Submitted
                }
            );
            assert_eq!(sink.inner.calls, 1);
            assert_eq!(
                sink.submit(operation),
                Submission::NotSubmitted(PlatformError::Permission)
            );
            assert_eq!(sink.inner.calls, 1);
        }
        finish(&mut watch);
    }
}
#[test]
fn x11_association_requires_selected_uid_and_exact_local_display() {
    let mut selected = selection();
    selected.uid = uid();
    let shared = Shared {
        selection: selected,
        state: AtomicU8::new(0),
        deadline: AtomicU64::new(0),
        waker: Mutex::new(None),
    };
    let control = Control(Arc::new(shared));
    assert!(control.matches_local_x11(":7"));
    assert!(!control.matches_local_x11(":8"));
    assert!(!control.matches_local_x11("localhost:7"));
}

#[test]
fn locked_gate_preserves_canonical_release_cleanup_and_unknown_obligations() {
    use fr_core::input::{InputEvent, InputRequest};
    use fr_core::input_submission::{Dispatch, Refusal};
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut watch = start(&peer);
    active(&watch);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut owner = input_support::session(&cx);
    let mut sink = Gate::new(Sink::default(), watch.control(), owner.monitor());
    let key = PhysicalKey::new(0xe1).unwrap();
    for (sequence, event) in [
        InputEvent::Key {
            key,
            transition: KeyTransition::Press,
        },
        InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 10, y: 10 },
            barrier: 0,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let result = owner
            .dispatch(
                InputRequest {
                    credentials: input_support::credentials(),
                    sequence: sequence as u64,
                    event,
                },
                &mut sink,
                || now(&cx),
            )
            .unwrap();
        let Dispatch::Completed(receipt) = result else {
            panic!("missing completed input")
        };
        assert_eq!(
            receipt.outcome,
            fr_core::input_sequence::InputOutcome::SubmittedToOs
        );
    }
    peer.emit(Event::LockUnlock);
    stopped(&watch, StopReason::Locked);
    assert_eq!(
        sink.submit(input_pair()[0]),
        Submission::NotSubmitted(PlatformError::Permission)
    );
    assert!(owner.monitor().is_revoked());
    // Even a remote release cannot bypass the canonical final authority check.
    let calls = sink.inner.calls;
    let refused = owner
        .dispatch(
            InputRequest {
                credentials: input_support::credentials(),
                sequence: 2,
                event: InputEvent::Key {
                    key,
                    transition: KeyTransition::Release,
                },
            },
            &mut sink,
            || now(&cx),
        )
        .unwrap();
    let Dispatch::Completed(refused) = refused else {
        panic!("missing refusal receipt")
    };
    assert_eq!(refused.refusal, Some(Refusal::Revoked));
    assert_eq!(refused.submitted_operations, 0);
    assert_eq!(sink.inner.calls, calls);
    sink.inner.unknown = true;
    let cleanup = owner.cleanup(&mut sink);
    assert_eq!(cleanup.submitted_releases, 0);
    assert_eq!(cleanup.remaining, 2);
    // An explicit owner cleanup retry, never an application input retry.
    sink.inner.unknown = false;
    let cleanup = owner.cleanup(&mut sink);
    assert_eq!(cleanup.submitted_releases, 2);
    assert_eq!(cleanup.remaining, 0);
    let calls = sink.inner.calls;
    assert_eq!(owner.cleanup(&mut sink).submitted_releases, 0);
    assert_eq!(sink.inner.calls, calls);
    assert_eq!(
        sink.prepare(input_pair()[0]),
        Err(PlatformError::Permission)
    );
    assert_eq!(
        sink.prepare(Operation::Text('x')),
        Err(PlatformError::Permission)
    );
    finish(&mut watch);
}

#[test]
fn abandoned_local_event_adapter_fences_its_source_not_the_broker() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut watch = start(&peer);
    active(&watch);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let broker = rt.request_cx_with_budget(Budget::INFINITE);
    let original = agent(1);
    drop(Events::new(watch.control(), &original, cx.clone()).unwrap());
    assert!(cx.is_cancel_requested());
    assert!(!broker.is_cancel_requested());
    assert_eq!(
        watch.control.status(),
        Status::Stopped(StopReason::OwnerStopped)
    );
    // Source-loop abandonment still owns its agent and native-release cleanup;
    // dropping this read-only association is not proof of completed cleanup.
    finish(&mut watch);
}
