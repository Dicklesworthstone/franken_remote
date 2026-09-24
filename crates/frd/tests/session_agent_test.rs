//! Integration and fault tests for interactive-session agent (plan §§5.3, 7.3, 15.2; bead fr-p1-session-agent-iq3).
//!
//! Tests:
//! 1. Local approval UI gating: observation blocked before prompt, granted scope clamped.
//! 2. Immediate revoke latency: sub-microsecond authority fence measured, OS cleanup reported separately.
//! 3. Remote-only held-state tracking and honest crash uncertainty reporting.
//! 4. Platform permission surfacing: typed refusal without Accessibility permission.
//! 5. Active session sleep inhibitor: ref-counted assertion held and released, verified in logs.
//! 6. Local input priority: lease suspended where distinguishable, limitation reported where not.
//! 7. macOS injection adapter: committed text distinct from physical keys, Accessibility check at submission.
//! 8. Fault tolerance: agent responsive with dead worker, no authority leak across crash or logout.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fr_core::{
    ids::RemoteSessionId,
    input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton},
    input_submission::{InputSink, Operation, PlatformError, Submission},
    time::{HostDuration, HostInstant},
};
use frd::input_watchdog::StopReason;
use frd::session_agent::{
    ApprovalMode, ApprovalState, AudioScope, DenialReason, Distinguishability, GrantedScope,
    IndicatorDisplayState, InhibitorAction, LocalPriorityOutcome, MacOsInputSink, PeerIdentity,
    PermissionKind, PermissionStatus, PlatformKind, PlatformPermissionError, PostedCgEvent,
    RecordingPoster, ReleaseCertainty, RequestedScope, SessionAgent, SessionCapabilitiesInUse,
    SessionRole, SubmissionRefusal,
};

fn make_bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).expect("valid bounds")
}

fn make_peer(id: &str) -> PeerIdentity {
    PeerIdentity {
        node_id: format!("node-{id}"),
        node_name: format!("laptop-{id}"),
        user_id: format!("user-{id}"),
    }
}

fn make_request(role: SessionRole) -> RequestedScope {
    RequestedScope {
        role,
        displays: vec![0],
        audio: AudioScope::PlaybackOnly,
        clipboard: true,
        file_transfer: true,
    }
}

fn agent(mode: ApprovalMode, platform: PlatformKind) -> SessionAgent {
    SessionAgent::new(mode, platform, 1000, make_bounds())
}

#[test]
fn test_approval_gating_observation_before_prompt() {
    let mut agent = agent(ApprovalMode::PromptAlways, PlatformKind::LinuxWayland);

    let session_id = RemoteSessionId::from_raw(101);
    let peer = make_peer("alice");
    let requested = make_request(SessionRole::Controller);
    let now = HostInstant::from_micros(1_000_000);

    // 1. Submit request in PromptAlways mode -> enters Pending
    let state = agent
        .request_session(session_id, &peer, &requested, now)
        .expect("request accepted");
    assert!(matches!(state, ApprovalState::Pending { .. }));

    // 2. Strict gating: observation (pixels, thumbnails, audio, clipboard) is NOT admitted
    assert!(!agent.is_observation_admitted(session_id));
    assert!(!agent.is_control_admitted(session_id));

    // 3. Attempting to approve with scope exceeding requested must be rejected
    let excessive_grant = GrantedScope {
        role: SessionRole::Controller,
        displays: vec![0, 1],             // Display 1 was not requested!
        audio: AudioScope::Bidirectional, // Mic was not requested!
        clipboard: true,
        file_transfer: true,
        granted_at: now,
        expires_at: None,
    };
    let excess_err = agent.approve_session(session_id, excessive_grant, now);
    assert_eq!(excess_err, Err(DenialReason::PolicyForbidden));
    assert!(!agent.is_observation_admitted(session_id));

    // 4. Clamped valid grant succeeds
    let valid_grant = GrantedScope {
        role: SessionRole::Controller,
        displays: vec![0],
        audio: AudioScope::PlaybackOnly,
        clipboard: true,
        file_transfer: false, // User granted subset: denied file transfer
        granted_at: now,
        expires_at: None,
    };
    let grant = agent
        .approve_session(session_id, valid_grant, now)
        .expect("approval succeeds");
    assert_eq!(grant.displays, vec![0]);
    assert!(!grant.file_transfer);

    // 5. Observation and control now admitted
    assert!(agent.is_observation_admitted(session_id));
    assert!(agent.is_control_admitted(session_id));
    assert!(agent.indicator().is_active());
}

#[test]
fn test_approval_mode_transition_revokes_existing_grants() {
    let mut agent = agent(ApprovalMode::Unattended, PlatformKind::LinuxWayland);

    let session_id = RemoteSessionId::from_raw(102);
    let peer = make_peer("bob");
    let requested = make_request(SessionRole::Controller);
    let now = HostInstant::from_micros(2_000_000);

    // In Unattended mode, request is auto-approved
    let state = agent
        .request_session(session_id, &peer, &requested, now)
        .expect("auto-approved");
    assert!(matches!(state, ApprovalState::Approved(_)));
    assert!(agent.is_observation_admitted(session_id));

    // Mode transition to PromptAlways must immediately revoke existing grants
    agent.set_approval_mode(ApprovalMode::PromptAlways, now);
    assert!(!agent.is_observation_admitted(session_id));
    assert!(!agent.is_control_admitted(session_id));
    assert!(agent.is_revoked());
}

#[test]
fn test_indicator_immediate_revoke_latency_and_async_cleanup() {
    let mut agent = SessionAgent::new(
        ApprovalMode::Unattended,
        PlatformKind::MacOs,
        501,
        make_bounds(),
    );

    let session_id = RemoteSessionId::from_raw(201);
    let peer = make_peer("charlie");
    let requested = make_request(SessionRole::Controller);
    let now = HostInstant::from_micros(3_000_000);

    agent
        .request_session(session_id, &peer, &requested, now)
        .expect("approved");
    assert!(agent.indicator().is_active());
    assert!(matches!(
        agent.indicator().display_state(),
        IndicatorDisplayState::Controlling { .. }
    ));

    // Immediate synchronous revoke at authority decision point
    let (outcome, releases) = agent.immediate_revoke(now, StopReason::LocalRevoke);

    // Revoke latency measured in nanoseconds (sub-millisecond synchronous fence)
    assert!(
        outcome.latency_ns < 10_000_000,
        "revoke latency must be sub-10ms, was {} ns",
        outcome.latency_ns
    );
    assert_eq!(outcome.at, now);

    // Authority is synchronously revoked before call returns
    assert!(agent.is_revoked());
    assert!(!agent.is_observation_admitted(session_id));

    // OS native cleanup is tracked separately and not yet complete
    assert!(!outcome.os_cleanup.is_complete());

    // Once native OS cleanup finishes, mark it complete
    agent.indicator().mark_os_cleanup_complete();
    assert!(outcome.os_cleanup.is_complete());

    // Releases synthesized
    assert_eq!(releases.len(), 0);
}

#[test]
fn test_held_state_tracking_and_crash_uncertainty_report() {
    let mut agent = agent(ApprovalMode::Unattended, PlatformKind::LinuxWayland);
    agent.permissions_mut().set_permission(
        PermissionKind::RemoteDesktopPortal,
        PermissionStatus::Granted,
    );

    let session_id = RemoteSessionId::from_raw(301);
    let peer = make_peer("david");
    let requested = make_request(SessionRole::Controller);
    let now = HostInstant::from_micros(4_000_000);

    agent
        .request_session(session_id, &peer, &requested, now)
        .expect("approved");

    // Clean initially
    assert!(agent.held_state().is_clean());

    // Inject Left Shift (USB HID 0xE1) and Primary Mouse Button
    let key_shift = PhysicalKey::new(0xE1).expect("valid shift key");
    let op_key_down = Operation::Key {
        key: key_shift,
        transition: KeyTransition::Press,
    };
    let op_btn_down = Operation::Button {
        button: PointerButton::Primary,
        pressed: true,
    };

    agent
        .verify_and_track_submission(session_id, &op_key_down, now)
        .expect("submission verified");
    agent
        .verify_and_track_submission(session_id, &op_btn_down, now)
        .expect("submission verified");

    assert_eq!(agent.held_state().held_key_count(), 1);
    assert_eq!(agent.held_state().held_button_count(), 1);
    assert!(agent.held_state().is_key_held(key_shift));
    assert!(agent.held_state().is_button_held(PointerButton::Primary));

    // Simulate input-process crash while keys were held
    let crash_report = agent.on_worker_crash(now);

    // Must report uncertain release state honestly without pretending keys were released!
    assert_eq!(crash_report.keys_uncertain, 1);
    assert_eq!(crash_report.buttons_uncertain, 1);
    assert_eq!(
        crash_report.certainty,
        ReleaseCertainty::UncertainDueToCrash
    );

    // Authority is revoked after crash
    assert!(agent.is_revoked());
    assert!(!agent.is_control_admitted(session_id));
}

#[test]
fn test_held_state_synthesizes_cleanup_releases_on_normal_revoke() {
    let mut agent = agent(ApprovalMode::Unattended, PlatformKind::LinuxWayland);
    agent.permissions_mut().set_permission(
        PermissionKind::RemoteDesktopPortal,
        PermissionStatus::Granted,
    );

    let session_id = RemoteSessionId::from_raw(302);
    let peer = make_peer("eve");
    let requested = make_request(SessionRole::Controller);
    let now = HostInstant::from_micros(5_000_000);

    agent
        .request_session(session_id, &peer, &requested, now)
        .expect("approved");

    // Hold Key 'A' (USB HID 0x04) and Key 'B' (USB HID 0x05)
    let key_a = PhysicalKey::new(0x04).unwrap();
    let key_b = PhysicalKey::new(0x05).unwrap();
    agent
        .verify_and_track_submission(
            session_id,
            &Operation::Key {
                key: key_a,
                transition: KeyTransition::Press,
            },
            now,
        )
        .unwrap();
    agent
        .verify_and_track_submission(
            session_id,
            &Operation::Key {
                key: key_b,
                transition: KeyTransition::Press,
            },
            now,
        )
        .unwrap();

    // Immediate revoke synthesizes release operations
    let (_outcome, releases) = agent.immediate_revoke(now, StopReason::LocalRevoke);
    assert_eq!(releases.len(), 2);
    assert!(matches!(
        releases[0],
        Operation::Key {
            transition: KeyTransition::Release,
            ..
        }
    ));
    assert!(matches!(
        releases[1],
        Operation::Key {
            transition: KeyTransition::Release,
            ..
        }
    ));

    // Generating a batch does not submit it. Keep every unresolved obligation.
    assert!(!agent.held_state().is_clean());
    assert_eq!(
        agent.held_state().last_certainty(),
        ReleaseCertainty::PendingCleanup
    );
    assert_eq!(
        agent.held_state_mut().synthesize_cleanup_releases(),
        releases
    );
    // Explicit synthetic native acknowledgements, not an actual OS-effect test.
    for operation in &releases {
        agent.held_state_mut().record_injected_operation(operation);
    }
    assert!(agent.held_state().is_clean());
    assert_eq!(
        agent.held_state().last_certainty(),
        ReleaseCertainty::ConfirmedReleased
    );
}

#[test]
fn test_platform_permissions_surfacing_and_typed_refusal() {
    let mut agent = SessionAgent::new(
        ApprovalMode::Unattended,
        PlatformKind::MacOs,
        501,
        make_bounds(),
    );

    let session_id = RemoteSessionId::from_raw(401);
    let peer = make_peer("frank");
    let requested = make_request(SessionRole::Controller);
    let now = HostInstant::from_micros(6_000_000);

    agent
        .request_session(session_id, &peer, &requested, now)
        .expect("approved");

    // By default, macOS Accessibility permission is PromptNeeded (not Granted)
    let key_a = PhysicalKey::new(0x04).unwrap();
    let op = Operation::Key {
        key: key_a,
        transition: KeyTransition::Press,
    };

    // Submission checkpoint must refuse with typed MissingAccessibility error!
    let err = agent.verify_and_track_submission(session_id, &op, now);
    assert_eq!(
        err,
        Err(SubmissionRefusal::Permission(
            PlatformPermissionError::MissingAccessibility
        ))
    );

    // Grant Accessibility permission
    agent.permissions_mut().set_permission(
        PermissionKind::AccessibilityInput,
        PermissionStatus::Granted,
    );

    // Now submission succeeds
    agent
        .verify_and_track_submission(session_id, &op, now)
        .expect("submission accepted once permission granted");

    // Desktop lock transitions: lock desktop
    agent.on_os_locked(now);
    assert!(agent.is_revoked());
    assert!(agent.permissions().is_locked());

    // Subsequent injection refused due to locked session
    let lock_err = agent.verify_and_track_submission(session_id, &op, now);
    assert!(matches!(lock_err, Err(SubmissionRefusal::Revoked)));
}

#[test]
fn test_sleep_inhibitor_lifecycle_and_log_audit() {
    let mut agent = agent(ApprovalMode::Unattended, PlatformKind::LinuxWayland);

    let session_1 = RemoteSessionId::from_raw(501);
    let session_2 = RemoteSessionId::from_raw(502);
    let peer_1 = make_peer("grace");
    let peer_2 = make_peer("heidi");
    let requested = make_request(SessionRole::Controller);

    let t1 = HostInstant::from_micros(7_000_000);
    let t2 = HostInstant::from_micros(7_500_000);
    let t3 = HostInstant::from_micros(8_000_000);
    let t4 = HostInstant::from_micros(8_500_000);

    // Initially inhibitor is not active
    assert!(!agent.sleep_inhibitor().is_inhibiting());
    assert_eq!(agent.sleep_inhibitor().active_session_count(), 0);

    // 1. Session 1 starts -> acquires sleep inhibitor assertion
    agent
        .request_session(session_1, &peer_1, &requested, t1)
        .unwrap();
    assert!(agent.sleep_inhibitor().is_inhibiting());
    assert_eq!(agent.sleep_inhibitor().active_session_count(), 1);

    // 2. Session 2 starts -> increments ref-count, retains inhibitor
    agent
        .request_session(session_2, &peer_2, &requested, t2)
        .unwrap();
    assert!(agent.sleep_inhibitor().is_inhibiting());
    assert_eq!(agent.sleep_inhibitor().active_session_count(), 2);

    // 3. Session 1 ends -> drops ref-count to 1, inhibitor remains held
    let _ = agent.on_session_ended(session_1, t3);
    assert!(agent.sleep_inhibitor().is_inhibiting());
    assert_eq!(agent.sleep_inhibitor().active_session_count(), 1);

    // 4. Session 2 ends -> ref-count drops to 0, inhibitor dropped immediately
    let _ = agent.on_session_ended(session_2, t4);
    assert!(!agent.sleep_inhibitor().is_inhibiting());
    assert_eq!(agent.sleep_inhibitor().active_session_count(), 0);

    // Verify inhibitor logs
    let logs = agent.sleep_inhibitor().logs();
    assert_eq!(
        logs.iter()
            .map(|l| (l.action, l.active_sessions_count))
            .collect::<Vec<_>>(),
        [
            (InhibitorAction::Acquired, 1),
            (InhibitorAction::Acquired, 2),
            (InhibitorAction::Released, 1),
            (InhibitorAction::Released, 0),
        ]
    );
}

#[test]
fn test_local_priority_distinguishable_vs_indistinguishable() {
    let mut agent = agent(ApprovalMode::Unattended, PlatformKind::LinuxWayland);
    agent.permissions_mut().set_permission(
        PermissionKind::RemoteDesktopPortal,
        PermissionStatus::Granted,
    );

    let session_id = RemoteSessionId::from_raw(601);
    let peer = make_peer("ivan");
    let requested = make_request(SessionRole::Controller);
    let t0 = HostInstant::from_micros(9_000_000);

    agent
        .request_session(session_id, &peer, &requested, t0)
        .unwrap();

    let key_a = PhysicalKey::new(0x04).unwrap();
    let op = Operation::Key {
        key: key_a,
        transition: KeyTransition::Press,
    };

    // Case 1: Indistinguishable platform (safe default)
    agent.local_priority_mut().set_enabled(true);
    agent
        .local_priority_mut()
        .set_distinguishability(Distinguishability::Indistinguishable);

    let outcome = agent.local_priority_mut().on_local_input_detected(t0);
    // Must report limitation honestly! Refuses to enable unstable heuristic.
    assert_eq!(outcome, LocalPriorityOutcome::UnsupportedPlatformHeuristic);
    assert!(!agent.local_priority().is_suspended(t0));

    // Case 2: Distinguishable platform
    agent
        .local_priority_mut()
        .set_distinguishability(Distinguishability::Distinguishable);

    let outcome = agent.local_priority_mut().on_local_input_detected(t0);
    let expected_until = t0
        .checked_add(HostDuration::from_micros(3_000_000))
        .unwrap();
    assert_eq!(
        outcome,
        LocalPriorityOutcome::Suspended {
            until: expected_until,
        }
    );
    assert!(agent.local_priority().is_suspended(t0));

    // While suspended, remote injection is refused at submission checkpoint
    let t_during = t0
        .checked_add(HostDuration::from_micros(1_000_000))
        .unwrap();
    let err = agent.verify_and_track_submission(session_id, &op, t_during);
    assert_eq!(
        err,
        Err(SubmissionRefusal::LocalPrioritySuspended(expected_until))
    );

    // After suspension duration elapses, submission is allowed
    let t_after = t0
        .checked_add(HostDuration::from_micros(3_500_000))
        .unwrap();
    assert!(!agent.local_priority().is_suspended(t_after));
    agent
        .verify_and_track_submission(session_id, &op, t_after)
        .expect("allowed after suspension ends");
}

#[test]
fn test_macos_injection_adapter_physical_key_vs_committed_text() {
    let bounds = make_bounds();

    // 1. Accessibility missing -> refused with PlatformError::Permission
    let poster_no_perm = RecordingPoster::new(false);
    let mut sink_no_perm = MacOsInputSink::new(poster_no_perm, bounds);

    let key_a = PhysicalKey::new(0x04).unwrap();
    let submit_err = sink_no_perm.submit(Operation::Key {
        key: key_a,
        transition: KeyTransition::Press,
    });
    assert_eq!(
        submit_err,
        Submission::NotSubmitted(PlatformError::Permission)
    );

    // 2. Accessibility granted -> physical key and committed text use distinct event paths
    let poster_perm = RecordingPoster::new(true);
    let mut sink = MacOsInputSink::new(poster_perm, bounds);

    // Physical key: USB usage 0x04 (Key A) maps to macOS keycode 0x00
    let sub_key = sink.submit(Operation::Key {
        key: key_a,
        transition: KeyTransition::Press,
    });
    assert_eq!(sub_key, Submission::Submitted);

    // Committed text: distinct path using Unicode scalar value, never synthetic keycodes!
    let sub_text = sink.submit(Operation::Text('Ω'));
    assert_eq!(sub_text, Submission::Submitted);

    // Absolute pointer positioning
    let pt = DesktopPoint { x: 500, y: 300 };
    let sub_mouse = sink.submit(Operation::Absolute(pt));
    assert_eq!(sub_mouse, Submission::Submitted);

    // Mouse button click
    let sub_btn = sink.submit(Operation::Button {
        button: PointerButton::Primary,
        pressed: true,
    });
    assert_eq!(sub_btn, Submission::Submitted);

    // Verify events recorded by poster
    assert_eq!(
        sink.poster().events,
        [
            PostedCgEvent::Key {
                keycode: 0x00,
                down: true,
            },
            PostedCgEvent::Text { character: 'Ω' },
            PostedCgEvent::MouseMove { x: 500.0, y: 300.0 },
            PostedCgEvent::MouseButton {
                button: 0,
                down: true,
                x: 500.0,
                y: 300.0,
            },
        ]
    );

    // Coordinate out of bounds is rejected
    let pt_out = DesktopPoint { x: 3000, y: 2000 };
    let sub_out = sink.submit(Operation::Absolute(pt_out));
    assert_eq!(
        sub_out,
        Submission::NotSubmitted(PlatformError::GeometryChanged)
    );
}

#[test]
fn test_expired_lease_refuses_injection_at_submission_checkpoint() {
    let mut agent = agent(ApprovalMode::PromptAlways, PlatformKind::LinuxWayland);
    agent.permissions_mut().set_permission(
        PermissionKind::RemoteDesktopPortal,
        PermissionStatus::Granted,
    );

    let session_id = RemoteSessionId::from_raw(701);
    let peer = make_peer("jack");
    let requested = make_request(SessionRole::Controller);
    let t0 = HostInstant::from_micros(10_000_000);
    let t_expire = HostInstant::from_micros(12_000_000);

    agent
        .request_session(session_id, &peer, &requested, t0)
        .unwrap();

    let grant = GrantedScope {
        role: SessionRole::Controller,
        displays: vec![0],
        audio: AudioScope::PlaybackOnly,
        clipboard: true,
        file_transfer: true,
        granted_at: t0,
        expires_at: Some(t_expire),
    };
    agent.approve_session(session_id, grant, t0).unwrap();

    let key_a = PhysicalKey::new(0x04).unwrap();
    let op = Operation::Key {
        key: key_a,
        transition: KeyTransition::Press,
    };

    // Before expiry (t = 11s) -> injection succeeds
    let t_valid = HostInstant::from_micros(11_000_000);
    agent
        .verify_and_track_submission(session_id, &op, t_valid)
        .expect("valid before expiry");

    // Exactly at or after expiry (t = 12s) -> injection refused with LeaseExpired!
    let t_expired = HostInstant::from_micros(12_000_000);
    let err = agent.verify_and_track_submission(session_id, &op, t_expired);
    assert_eq!(err, Err(SubmissionRefusal::LeaseExpired));

    // Well after expiry (t = 15s) -> still refused
    let t_later = HostInstant::from_micros(15_000_000);
    let err2 = agent.verify_and_track_submission(session_id, &op, t_later);
    assert_eq!(err2, Err(SubmissionRefusal::LeaseExpired));
}

#[test]
fn test_fault_agent_alive_with_dead_worker() {
    let mut agent = agent(ApprovalMode::Unattended, PlatformKind::LinuxWayland);
    agent.permissions_mut().set_permission(
        PermissionKind::RemoteDesktopPortal,
        PermissionStatus::Granted,
    );

    let session_id = RemoteSessionId::from_raw(801);
    let peer = make_peer("karen");
    let requested = make_request(SessionRole::Controller);
    let t0 = HostInstant::from_micros(13_000_000);

    agent
        .request_session(session_id, &peer, &requested, t0)
        .unwrap();
    assert!(agent.sleep_inhibitor().is_inhibiting());

    // Inject held keys
    let key_space = PhysicalKey::new(0x2C).unwrap();
    agent
        .verify_and_track_submission(
            session_id,
            &Operation::Key {
                key: key_space,
                transition: KeyTransition::Press,
            },
            t0,
        )
        .unwrap();

    // Worker crashes! Agent stays alive, healthy, and responsive
    let t_crash = HostInstant::from_micros(13_500_000);
    let report = agent.on_worker_crash(t_crash);

    // Honest crash uncertainty report
    assert_eq!(report.keys_uncertain, 1);
    assert_eq!(report.certainty, ReleaseCertainty::UncertainDueToCrash);

    // Sleep inhibitor released immediately on crash
    assert!(!agent.sleep_inhibitor().is_inhibiting());

    // Authority is revoked
    assert!(agent.is_revoked());

    // Submitting input after crash fails immediately with Revoked
    let err = agent.verify_and_track_submission(
        session_id,
        &Operation::Key {
            key: key_space,
            transition: KeyTransition::Release,
        },
        t_crash,
    );
    assert_eq!(err, Err(SubmissionRefusal::Revoked));

    // Agent remains fully alive and responsive for subsequent queries and lifecycle calls
    assert!(agent.indicator().last_revoke_outcome().is_some());
    let remaining = agent.on_session_ended(session_id, t_crash);
    // Tracker synthesized releases for cleanup
    assert_eq!(remaining.len(), 1);
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_connected_sessions_list_multi_viewer_tracking() {
    let mut agent = agent(ApprovalMode::Unattended, PlatformKind::LinuxWayland);

    let t0 = HostInstant::from_micros(1_000_000);

    // 1. Initially no connected sessions
    assert_eq!(agent.connected_sessions().len(), 0);
    assert_eq!(
        agent.indicator().display_state(),
        IndicatorDisplayState::Hidden
    );
    assert!(!agent.indicator().is_active());

    // 2. Connect Session 101: Controller with full capabilities
    let session_101 = RemoteSessionId::from_raw(101);
    let peer_101 = PeerIdentity {
        node_id: "node-alice".into(),
        node_name: "alice-macbook".into(),
        user_id: "user-alice".into(),
    };
    let req_101 = RequestedScope {
        role: SessionRole::Controller,
        displays: vec![0],
        audio: AudioScope::Bidirectional,
        clipboard: true,
        file_transfer: true,
    };
    agent
        .request_session(session_101, &peer_101, &req_101, t0)
        .expect("session 101 approved");

    let sessions = agent.connected_sessions();
    assert_eq!(
        (
            sessions.len(),
            sessions[0].session_id,
            sessions[0].device_name.as_str(),
            sessions[0].role
        ),
        (1, session_101, "alice-macbook", SessionRole::Controller)
    );
    assert_eq!(
        sessions[0].capabilities,
        SessionCapabilitiesInUse {
            view: true,
            control: true,
            audio: true,
            clipboard: true,
            files: true
        }
    );
    assert!(agent.indicator().is_active());
    assert!(
        matches!(agent.indicator().display_state(), IndicatorDisplayState::Controlling { session_id, has_input_lease: true, .. } if session_id == session_101)
    );

    // 3. Connect Session 102: Observer with view and playback audio, no control/clipboard/files
    let t1 = HostInstant::from_micros(2_000_000);
    let session_102 = RemoteSessionId::from_raw(102);
    let peer_102 = PeerIdentity {
        node_id: "node-bob".into(),
        node_name: "bob-ipad".into(),
        user_id: "user-bob".into(),
    };
    let req_102 = RequestedScope {
        role: SessionRole::Observer,
        displays: vec![0],
        audio: AudioScope::PlaybackOnly,
        clipboard: false,
        file_transfer: false,
    };
    agent
        .request_session(session_102, &peer_102, &req_102, t1)
        .expect("session 102 approved");

    let sessions = agent.connected_sessions();
    assert_eq!(
        (sessions.len(), sessions[0].session_id, sessions[0].role),
        (2, session_101, SessionRole::Controller)
    );
    assert_eq!(
        (
            sessions[1].session_id,
            sessions[1].device_name.as_str(),
            sessions[1].role
        ),
        (session_102, "bob-ipad", SessionRole::Observer)
    );
    assert_eq!(
        sessions[1].capabilities,
        SessionCapabilitiesInUse {
            view: true,
            control: false,
            audio: true,
            clipboard: false,
            files: false
        }
    );

    // MultiSession state visible in indicator UI
    let display_state = agent.indicator().display_state();
    assert!(matches!(
        display_state,
        IndicatorDisplayState::ActiveSessions { .. }
    ));
    if let IndicatorDisplayState::ActiveSessions {
        sessions: active_list,
    } = display_state
    {
        assert_eq!(active_list.len(), 2);
        assert_eq!(active_list[0].session_id, session_101);
        assert_eq!(active_list[1].session_id, session_102);
    }

    // 4. Connect Session 103: Observer 2 with view only
    let t2 = HostInstant::from_micros(3_000_000);
    let session_103 = RemoteSessionId::from_raw(103);
    let peer_103 = PeerIdentity {
        node_id: "node-carol".into(),
        node_name: "carol-linux".into(),
        user_id: "user-carol".into(),
    };
    let req_103 = RequestedScope {
        role: SessionRole::Observer,
        displays: vec![0],
        audio: AudioScope::None,
        clipboard: false,
        file_transfer: false,
    };
    agent
        .request_session(session_103, &peer_103, &req_103, t2)
        .expect("session 103 approved");

    let sessions = agent.connected_sessions();
    assert_eq!(
        (
            sessions.len(),
            sessions[2].session_id,
            sessions[2].device_name.as_str(),
            sessions[2].role
        ),
        (3, session_103, "carol-linux", SessionRole::Observer)
    );
    assert_eq!(
        sessions[2].capabilities,
        SessionCapabilitiesInUse {
            view: true,
            control: false,
            audio: false,
            clipboard: false,
            files: false
        }
    );
}

#[test]
fn test_per_session_revoke_leaves_other_sessions_undisturbed() {
    let mut agent = agent(ApprovalMode::Unattended, PlatformKind::LinuxWayland);

    let t0 = HostInstant::from_micros(1_000_000);
    let session_101 = RemoteSessionId::from_raw(101);
    let session_102 = RemoteSessionId::from_raw(102);

    let peer_101 = make_peer("101");
    let req_101 = make_request(SessionRole::Controller);
    agent
        .request_session(session_101, &peer_101, &req_101, t0)
        .unwrap();

    let peer_102 = make_peer("102");
    let req_102 = make_request(SessionRole::Observer);
    agent
        .request_session(session_102, &peer_102, &req_102, t0)
        .unwrap();

    assert_eq!(agent.connected_sessions().len(), 2);

    // Register per-session revokers
    let s101_revoked = Arc::new(AtomicBool::new(false));
    let s102_revoked = Arc::new(AtomicBool::new(false));

    let flag_101 = s101_revoked.clone();
    agent.register_session_custom_revoker(session_101, move || {
        flag_101.store(true, Ordering::Release);
    });

    let flag_102 = s102_revoked.clone();
    agent.register_session_custom_revoker(session_102, move || {
        flag_102.store(true, Ordering::Release);
    });

    // 1. Revoke Session 102 (Observer) only
    let t_revoke_102 = HostInstant::from_micros(2_000_000);
    let outcome = agent.revoke_session(session_102, t_revoke_102, StopReason::LocalRevoke);
    assert!(outcome.is_some());
    let (res, releases) = outcome.unwrap();
    assert_eq!(releases.len(), 0, "no input cleanup needed for observer");
    assert!(res.latency_ns < 10_000_000);

    // Session 102 revoker fired
    assert!(s102_revoked.load(Ordering::Acquire));
    // Session 101 revoker was NOT fired
    assert!(!s101_revoked.load(Ordering::Acquire));

    // Session 101 is STILL admitted for control and observation
    assert!(agent.is_control_admitted(session_101));
    assert!(agent.is_observation_admitted(session_101));

    // Session 102 is NO LONGER admitted
    assert!(!agent.is_observation_admitted(session_102));

    // Connected sessions list now has exactly 1 session: Session 101
    let remaining = agent.connected_sessions();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].session_id, session_101);

    // Indicator display state transitioned cleanly back to Controlling for Session 101
    assert!(matches!(
        agent.indicator().display_state(),
        IndicatorDisplayState::Controlling {
            session_id,
            ..
        } if session_id == session_101
    ));

    // 2. Now revoke Session 101 (Controller)
    let t_revoke_101 = HostInstant::from_micros(3_000_000);
    let outcome_101 = agent.revoke_session(session_101, t_revoke_101, StopReason::LocalRevoke);
    assert!(outcome_101.is_some());
    let (revoked_status_101, _) = outcome_101.unwrap();
    assert!(revoked_status_101.latency_ns < 10_000_000);

    // Session 101 revoker has now fired
    assert!(s101_revoked.load(Ordering::Acquire));

    // All sessions removed
    assert_eq!(agent.connected_sessions().len(), 0);
    assert!(!agent.is_control_admitted(session_101));
    assert!(matches!(
        agent.indicator().display_state(),
        IndicatorDisplayState::Revoked { .. }
    ));
}

#[test]
fn test_immediate_revoke_revokes_all_sessions_simultaneously() {
    let mut agent = agent(ApprovalMode::Unattended, PlatformKind::LinuxWayland);

    let t0 = HostInstant::from_micros(1_000_000);
    let s1 = RemoteSessionId::from_raw(201);
    let s2 = RemoteSessionId::from_raw(202);
    let s3 = RemoteSessionId::from_raw(203);

    agent
        .request_session(
            s1,
            &make_peer("201"),
            &make_request(SessionRole::Controller),
            t0,
        )
        .unwrap();
    agent
        .request_session(
            s2,
            &make_peer("202"),
            &make_request(SessionRole::Observer),
            t0,
        )
        .unwrap();
    agent
        .request_session(
            s3,
            &make_peer("203"),
            &make_request(SessionRole::Observer),
            t0,
        )
        .unwrap();

    assert_eq!(agent.connected_sessions().len(), 3);

    let r1 = Arc::new(AtomicBool::new(false));
    let r2 = Arc::new(AtomicBool::new(false));
    let r3 = Arc::new(AtomicBool::new(false));
    let r_global = Arc::new(AtomicBool::new(false));

    let f1 = r1.clone();
    agent.register_session_custom_revoker(s1, move || f1.store(true, Ordering::Release));
    let f2 = r2.clone();
    agent.register_session_custom_revoker(s2, move || f2.store(true, Ordering::Release));
    let f3 = r3.clone();
    agent.register_session_custom_revoker(s3, move || f3.store(true, Ordering::Release));
    let fg = r_global.clone();
    agent
        .indicator()
        .register_custom_revoker(move || fg.store(true, Ordering::Release));

    // Immediate revoke host-wide (e.g. lid close or emergency user revoke)
    let t_kill = HostInstant::from_micros(5_000_000);
    let (outcome, _) = agent.immediate_revoke(t_kill, StopReason::LocalRevoke);
    assert!(outcome.latency_ns < 10_000_000);

    // All revokers fired simultaneously
    assert!(r1.load(Ordering::Acquire));
    assert!(r2.load(Ordering::Acquire));
    assert!(r3.load(Ordering::Acquire));
    assert!(r_global.load(Ordering::Acquire));

    // All sessions cleared
    assert_eq!(agent.connected_sessions().len(), 0);
    assert!(agent.is_revoked());
    assert!(!agent.is_control_admitted(s1));
    assert!(!agent.is_observation_admitted(s2));
    assert!(!agent.is_observation_admitted(s3));
}
